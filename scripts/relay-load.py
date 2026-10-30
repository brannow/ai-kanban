#!/usr/bin/env python3
"""The probe behind scripts/relay-load: N notification streams over R rooms against a local relay.

  relay-load.py <port> <key> <relay pid> <N> <rooms>

Reports how many streams connected, the relay's CPU while idle, its open files, and for five
pushes to room r0 the time until every r0 stream saw the new head. Standard library only.
"""
import json, re, resource, selectors, socket, subprocess, sys, time, urllib.request

port, key, pid, N, R = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5])
seq0 = 1
soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
resource.setrlimit(resource.RLIMIT_NOFILE, (min(8192, hard), hard))
base = f"http://127.0.0.1:{port}"

def req(method, path, body=None):
    r = urllib.request.Request(base + path, method=method, data=None if body is None else json.dumps(body).encode(),
                               headers={"authorization": f"Bearer {key}", "content-type": "application/json"})
    with urllib.request.urlopen(r, timeout=30) as res:
        return json.loads(res.read() or b"null")

def cpu():
    out = subprocess.run(["ps", "-o", "time=", "-p", pid], capture_output=True, text=True).stdout.strip()
    parts = [float(p) for p in re.split(r"[:]", out)]
    return sum(v * 60 ** i for i, v in enumerate(reversed(parts)))

def fds():
    return len(subprocess.run(["lsof", "-p", pid], capture_output=True, text=True).stdout.splitlines()) - 1

tag = req("GET", "/v1/whoami")["tag"]
for r in range(R):
    req("POST", f"/v1/rooms/r{r}/join")

sel = selectors.DefaultSelector()
streams = {}  # sock -> [room, buffer, last head]
failed = 0
busy = 0  # answered 503: the relay's stream cap, not a failure
for i in range(N):
    room = f"r{i % R}"
    try:
        s = socket.create_connection(("127.0.0.1", int(port)), timeout=5)
        s.sendall(f"GET /v1/rooms/{room}/events HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {key}\r\n\r\n".encode())
        s.setblocking(False)
        sel.register(s, selectors.EVENT_READ)
        streams[s] = [room, b"", None, False]
    except OSError:
        failed += 1

def pump(until, want=None):
    """Read streams until `until`; with `want` (room, head), return the time all of that room's streams saw it."""
    global busy
    while time.monotonic() < until:
        for k, _ in sel.select(timeout=0.05):
            s = k.fileobj
            try:
                data = s.recv(65536)
            except OSError:
                data = b""
            st = streams[s]
            if data.startswith(b"HTTP/1.1 503"):
                st[3] = True
            if not data:
                busy += st[3]
                sel.unregister(s); s.close(); del streams[s]
                continue
            st[1] += data
            for m in re.finditer(rb"data: (\d+)", st[1]):
                st[2] = int(m.group(1))
            st[1] = st[1][-64:]
        if want and all(st[2] is not None and st[2] >= want[1] for st in streams.values() if st[0] == want[0]):
            return time.monotonic()
    return None

pump(time.monotonic() + 3)
alive = len(streams)
c0 = cpu(); pump(time.monotonic() + 10); idle_cpu = (cpu() - c0) / 10 * 100
fd_count = fds()

lat = []
for k in range(5):
    seq = seq0 + k
    change = {"cid": f"{tag}:{seq}", "entity": {"kind": "task", "origin": tag, "id": seq}, "field": "title",
              "value": f"load {seq}", "clock": {"wall_ms": 1000 + seq, "counter": 0, "tag": tag}}
    pump(time.monotonic() + 0.3 + (k * 0.17) % 1)  # land at varying points of the 1s tick
    t0 = time.monotonic()
    head = req("POST", "/v1/rooms/r0/changes", {"changes": [change]})
    target = head.get("head") if isinstance(head, dict) and "head" in head else None
    if target is None:
        target = req("GET", "/v1/rooms/r0/changes?since=0&limit=1")["head"]
    done = pump(time.monotonic() + 5, ("r0", target))
    lat.append(None if done is None else (done - t0) * 1000)

print(f"N={N} rooms={R}: connected {alive}/{N} (busy 503 {busy}, failed {failed}, dropped {N - failed - alive - busy}), "
      f"idle cpu {idle_cpu:.0f}%, fds {fd_count}, push->all r0 streams ms {[None if x is None else round(x) for x in lat]}", flush=True)
