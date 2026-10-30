#!/usr/bin/env python3
"""The probe behind scripts/sync-smoke: create a task through client A's web API and time how long
until client B's web API returns it, with A's title and body.

B is followed the way the page follows it: its /api/stream, and on every event a read of its tasks
(plus a read every 2s, in case an event was missed). Standard library only.

  sync-smoke.py <port A> <port B> <room> <label> <gap>:<runs> [<gap>:<runs> ...]
"""
import json, sys, threading, time, urllib.request, uuid

port_a, port_b, room, label = sys.argv[1:5]
plan = [(float(g.split(":")[0]), int(g.split(":")[1])) for g in sys.argv[5:]]
A, B = f"http://127.0.0.1:{port_a}", f"http://127.0.0.1:{port_b}"


def get(url):
    with urllib.request.urlopen(url, timeout=10) as r:
        return json.loads(r.read())


def post(url, body):
    req = urllib.request.Request(url, data=json.dumps(body).encode(), method="POST", headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read())


def find(v, title):
    if isinstance(v, dict):
        if v.get("title") == title:
            return v
        v = list(v.values())
    if isinstance(v, list):
        for x in v:
            hit = find(x, title)
            if hit:
                return hit
    return None


def board(base):
    for p in get(f"{base}/api/projects")["projects"]:
        if p.get("room") == room:
            return p["project"]["id"]
    sys.exit(f"no board in room {room} at {base}")


board_a, board_b = board(A), board(B)
wake = threading.Event()


def stream():
    with urllib.request.urlopen(f"{B}/api/stream", timeout=3600) as r:
        for line in r:
            if line.startswith((b"data:", b"event:")):
                wake.set()


threading.Thread(target=stream, daemon=True).start()
time.sleep(1)

results = []
for gap, runs in plan:
    for i in range(runs):
        time.sleep(gap)
        title = f"smoke {label} gap{gap} #{i} {uuid.uuid4().hex[:6]}"
        body = f"body from A {uuid.uuid4().hex}"
        wake.clear()
        t0 = time.monotonic()
        post(f"{A}/api/projects/{board_a}/tasks", {"title": title, "body": body})
        seen = None
        while time.monotonic() - t0 < 30:
            wake.wait(timeout=2)
            wake.clear()
            hit = find(get(f"{B}/api/projects/{board_b}/tasks?limit=200"), title)
            if hit:
                seen = (time.monotonic() - t0, hit)
                break
        if not seen:
            print(f"{label} gap {gap:>5}s #{i}: NOT SEEN within 30s", flush=True)
            results.append((gap, None))
            continue
        took, hit = seen
        full = get(f"{B}/api/projects/{board_b}/tasks/{hit.get('id')}")
        ok = find(full, title) is not None and body in json.dumps(full)
        print(f"{label} gap {gap:>5}s #{i}: on B after {took:5.2f}s  (data {'ok' if ok else 'MISMATCH'})", flush=True)
        results.append((gap, took))

for gap in sorted({g for g, _ in results}):
    ts = sorted(t for g, t in results if g == gap and t is not None)
    lost = sum(1 for g, t in results if g == gap and t is None)
    if ts:
        print(f"{label} gap {gap}s: n={len(ts)} min={ts[0]:.2f} median={ts[len(ts) // 2]:.2f} max={ts[-1]:.2f}" + (f" NOT SEEN {lost}" if lost else ""))
