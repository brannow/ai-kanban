#!/usr/bin/env python3
"""The probe behind scripts/relay-stress: W writers pushing and pulling at once, plus idle streams.

  relay-stress.py <port> <relay pid> <seconds> <streams> <key> [<key> ...]

The first key holds every stream; each further key is one writer, with a tag of its own as a
real member has. Writers push one task change and pull, in a loop, with no pause: the point is
contention on the relay's single database lock (#1090). Streams sit in the writers' rooms and in
a room nobody writes to, whose heartbeats show whether the relay's timers keep running under
load. Standard library only.
"""
import json, re, resource, selectors, socket, statistics, subprocess, sys, threading, time, urllib.request

port, pid, secs, S = sys.argv[1], sys.argv[2], float(sys.argv[3]), int(sys.argv[4])
# Rounds per second per writer; 0 is no pause at all (the relay's ceiling, not people).
RATE = float(__import__("os").environ.get("RELAY_STRESS_RATE", "0"))
stream_key, writer_keys = sys.argv[5], sys.argv[6:]
W, ROOMS = len(writer_keys), 10
soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
resource.setrlimit(resource.RLIMIT_NOFILE, (min(16384, hard), hard))
base = f"http://127.0.0.1:{port}"


def req(key, method, path, body=None):
    r = urllib.request.Request(base + path, method=method, data=None if body is None else json.dumps(body).encode(),
                               headers={"authorization": f"Bearer {key}", "content-type": "application/json"})
    with urllib.request.urlopen(r, timeout=60) as res:
        return json.loads(res.read() or b"null")


def cpu_seconds():
    out = subprocess.run(["ps", "-o", "time=", "-p", pid], capture_output=True, text=True).stdout.strip()
    return sum(float(v) * 60 ** i for i, v in enumerate(reversed(out.split(":"))))


def fds():
    return len(subprocess.run(["lsof", "-p", pid], capture_output=True, text=True).stdout.splitlines()) - 1


def pct(xs, p):
    if not xs:
        return None
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


# --- streams: in every writers' room, and in "quiet", where nobody writes -------------------------
for r in [f"r{i}" for i in range(ROOMS)] + ["quiet"]:
    req(stream_key, "POST", f"/v1/rooms/{r}/join")
sel = selectors.DefaultSelector()
streams = {}  # sock -> {"room", "buf", "arrivals": [(t, head)], "bytes_at": [t]}
refused = 0
quiet_n = max(10, S // 20)
for i in range(S):
    room = "quiet" if i < quiet_n else f"r{i % ROOMS}"
    try:
        s = socket.create_connection(("127.0.0.1", int(port)), timeout=5)
        s.sendall(f"GET /v1/rooms/{room}/events HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {stream_key}\r\n\r\n".encode())
        s.setblocking(False)
        sel.register(s, selectors.EVENT_READ)
        streams[s] = {"room": room, "buf": b"", "arrivals": [], "bytes_at": []}
    except OSError:
        refused += 1

stop = threading.Event()
# The reader keeps going a little after the writers stop, so the last pushes count as seen.
reading = threading.Event()
dropped = 0


def pump():
    global dropped
    while not reading.is_set():
        for k, _ in sel.select(timeout=0.05):
            s, now = k.fileobj, time.monotonic()
            try:
                data = s.recv(65536)
            except OSError:
                data = b""
            st = streams[s]
            if not data:
                dropped += 1
                sel.unregister(s)
                continue
            st["bytes_at"].append(now)
            st["buf"] += data
            for m in re.finditer(rb"data: (\d+)", st["buf"]):
                st["arrivals"].append((now, int(m.group(1))))
            st["buf"] = st["buf"][-64:]


reader = threading.Thread(target=pump, daemon=True)
reader.start()
time.sleep(2)

# --- writers ---------------------------------------------------------------------------------
pushes, pulls, errors = [], [], []
heads = []  # (room, head after this push, monotonic time the push was sent)
lock = threading.Lock()


def writer(i, key):
    room = f"r{i % ROOMS}"
    try:
        tag = req(key, "GET", "/v1/whoami")["tag"]
        req(key, "POST", f"/v1/rooms/{room}/join")
    except Exception as e:  # noqa: BLE001 -- counted, not raised: one failed writer must not end the run
        with lock:
            errors.append(f"setup: {e}")
        return
    seq, cursor = 0, 0
    next_at = time.monotonic() + (i / W / RATE if RATE else 0)  # spread the writers' starts
    while not stop.is_set():
        if RATE:
            stop.wait(max(0.0, next_at - time.monotonic()))
            next_at += 1 / RATE
            if stop.is_set():
                break
        seq += 1
        change = {"cid": f"{tag}:{seq}", "entity": {"kind": "task", "origin": tag, "id": seq}, "field": "title",
                  "value": f"stress {seq}", "clock": {"wall_ms": int(time.time() * 1000), "counter": 0, "tag": tag}}
        t0 = time.monotonic()
        try:
            got = req(key, "POST", f"/v1/rooms/{room}/changes", {"changes": [change]})
            t1 = time.monotonic()
            page = req(key, "GET", f"/v1/rooms/{room}/changes?since={cursor}&limit=1000")
            t2 = time.monotonic()
        except Exception as e:  # noqa: BLE001
            with lock:
                errors.append(str(e)[:80])
            continue
        cursor = max([cursor] + [c["seq"] for c in page["changes"]])
        with lock:
            pushes.append(t1 - t0)
            pulls.append(t2 - t1)
            heads.append((room, got["head"], t0))


c0, w0 = cpu_seconds(), time.monotonic()
threads = [threading.Thread(target=writer, args=(i, k), daemon=True) for i, k in enumerate(writer_keys)]
for t in threads:
    t.start()
time.sleep(secs)
stop.set()
for t in threads:
    t.join(timeout=70)
cpu = (cpu_seconds() - c0) / (time.monotonic() - w0) * 100
open_files = fds()
time.sleep(1.5)
reading.set()

# --- push -> every stream in its room: from sending the push to the last stream seeing its head --
by_room = {}
for st in streams.values():
    by_room.setdefault(st["room"], []).append(st["arrivals"])
deliver, missed = [], 0
for room, head, t in heads[:: max(1, len(heads) // 400)]:
    worst = 0.0
    for arrivals in by_room.get(room, []):
        seen = next((at for at, h in arrivals if h >= head), None)
        if seen is None:
            missed += 1
            worst = None
            break
        worst = max(worst, seen - t)
    if worst is not None:
        deliver.append(max(0.0, worst))

# --- heartbeats in the quiet room: the largest silence on any of its streams --------------------
gaps = []
for st in streams.values():
    if st["room"] == "quiet" and len(st["bytes_at"]) > 1:
        ts = st["bytes_at"]
        gaps.append(max(b - a for a, b in zip(ts, ts[1:])))

ms = lambda x: "-" if x is None else f"{x * 1000:.0f}"
n = len(pushes)
print(f"W={W:<3} streams={len(streams)} (refused {refused}, dropped {dropped}) | "
      f"rounds {n} ({n / secs:.0f}/s), errors {len(errors)} | "
      f"push ms p50 {ms(pct(pushes, 50))} p99 {ms(pct(pushes, 99))} max {ms(max(pushes) if pushes else None)} | "
      f"pull ms p50 {ms(pct(pulls, 50))} p99 {ms(pct(pulls, 99))} | "
      f"push->all streams ms p50 {ms(pct(deliver, 50))} p99 {ms(pct(deliver, 99))} (missed {missed}) | "
      f"quiet-room heartbeat gap max {f'{max(gaps):.1f}s' if gaps else '-'} | relay cpu {cpu:.0f}% fds {open_files}", flush=True)
if errors:
    print("  first errors:", sorted(set(errors))[:3], flush=True)
