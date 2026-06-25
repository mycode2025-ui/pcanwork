"""pcanwork.py — PcanWork test-automation client (pure stdlib, Python 3.7+).

Talks to a running PcanWork instance over TCP loopback using newline-delimited
JSON. PcanWork launches your script DIRECTLY (python your_script.py) with this
module's directory on PYTHONPATH and two env vars set:
    PCANWORK_IPC_PORT   the ephemeral port the app is listening on
    PCANWORK_IPC_TOKEN  a per-session 32-hex token used in the handshake
You normally just call:  with pcanwork.connect() as pcan: ...

No pip installs. Works with ANY CPython 3.7+ with a normal site config
(socket/json/dataclasses/queue/threading are all stdlib).
"""
from __future__ import annotations
import os, sys, json, time, socket, threading, queue
from dataclasses import dataclass
from typing import Optional, Callable, Dict, List, Any

PROTO = 1


# ----------------------------- exceptions -----------------------------------
class PcanError(Exception):
    """Base for all client errors."""


class ConnectError(PcanError):
    """Handshake / socket setup failed."""


class ProtocolError(PcanError):
    pass


class NotConnectedError(PcanError):
    pass


class TimeoutError_(PcanError):
    pass


class RemoteError(PcanError):
    def __init__(self, code: str, msg: str = ""):
        super().__init__(f"{code}: {msg}" if msg else code)
        self.code = code
        self.msg = msg


# code -> specialized class; ALL carry the server message (red-team minor fix:
# never discard err.msg, both branches build consistently).
_ERR_MAP = {
    "NOT_CONNECTED": NotConnectedError,
    "PROTOCOL": ProtocolError,
    "TIMEOUT": TimeoutError_,
}


def _make_error(code: str, msg: str) -> PcanError:
    cls = _ERR_MAP.get(code)
    if cls is None:
        return RemoteError(code, msg)
    e = cls(msg)            # NotConnectedError/ProtocolError/TimeoutError_ take a message
    setattr(e, "code", code)
    setattr(e, "msg", msg)
    return e


# ----------------------------- frame ----------------------------------------
@dataclass
class Frame:
    ch: int
    id: int
    data: bytes
    t: float = 0.0
    count: int = 0
    ext: bool = False
    fd: bool = False
    brs: bool = False
    remote: bool = False
    tx: bool = False

    @staticmethod
    def _from(d: Dict[str, Any]) -> "Frame":
        return Frame(
            ch=d.get("ch", 0), id=d.get("id", 0),
            data=bytes(d.get("data", []) or []),
            t=d.get("t", 0.0), count=d.get("count", 0),
            ext=d.get("ext", False), fd=d.get("fd", False),
            brs=d.get("brs", False), remote=d.get("remote", False),
            tx=d.get("tx", False),
        )

    def __repr__(self) -> str:
        return (f"Frame(ch={self.ch}, id=0x{self.id:X}, "
                f"data={self.data.hex(' ')}, t={self.t:.3f}, "
                f"count={self.count}, tx={self.tx})")


def _check_ch(ch: int) -> int:
    """Software channel must be 1..255 (CANx). Reject out-of-range early instead
    of letting the app silently truncate to u8 and route to the wrong channel."""
    if not isinstance(ch, int) or isinstance(ch, bool) or not (1 <= ch <= 255):
        raise ValueError(f"ch must be an int in 1..255 (CANx software channel), got {ch!r}")
    return ch


# ----------------------------- session --------------------------------------
class Session:
    """One connection to the running PcanWork app. Use as a context manager."""

    def __init__(self, host: str, port: int, token: str, timeout: float = 5.0):
        self._seq = 1
        self._lock = threading.Lock()          # serialize request/response
        self._evt_subs: List[tuple] = []       # (ids_set, predicate, out_queue)
        self._evt_lock = threading.Lock()
        self.passed = 0
        self.failed = 0
        self._dropped = 0
        self._closed = False
        try:
            self._sock = socket.create_connection((host, port), timeout=timeout)
        except OSError as e:
            raise ConnectError(f"cannot reach PcanWork at {host}:{port}: {e}")
        self._sock.settimeout(None)
        self._rf = self._sock.makefile("r", encoding="utf-8", newline="\n")
        # per-id reply mailbox so a late/stale reply never satisfies the wrong call
        self._replies: "Dict[int, queue.Queue[dict]]" = {}
        self._replies_lock = threading.Lock()
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()
        # handshake; the app auto-subscribes this connection to ALL frames at
        # hello time, so send-then-wait is race-free without an explicit subscribe.
        r = self._call("hello", token=token)
        if r.get("proto") != PROTO:
            self.log(f"warning: proto mismatch app={r.get('proto')} client={PROTO}")
        self._subscribed_all = True            # server default after hello

    # ---- low-level IO ----
    def _read_loop(self):
        try:
            for line in self._rf:
                line = line.strip()
                if not line:
                    continue
                try:
                    m = json.loads(line)
                except ValueError:
                    continue
                ev = m.get("event")
                if ev:                            # unsolicited event
                    if ev == "frame":
                        self._dispatch_event(Frame._from(m.get("data", {})))
                    elif ev == "dropped":
                        self._dropped += int(m.get("data", {}).get("n", 0))
                    continue
                mid = m.get("id")                 # response -> route by id
                with self._replies_lock:
                    q = self._replies.get(mid)
                if q is not None:
                    q.put(m)
        except Exception:
            pass
        finally:
            self._closed = True
            with self._replies_lock:
                for q in self._replies.values():
                    q.put({"__eof__": True})

    def _dispatch_event(self, fr: Frame):
        with self._evt_lock:
            subs = list(self._evt_subs)
        for ids, pred, outq in subs:
            if ids and fr.id not in ids:
                continue
            if pred and not pred(fr):
                continue
            outq.put(fr)

    def _send_line(self, obj: dict):
        if self._closed:
            raise ConnectError("connection closed by PcanWork")
        data = (json.dumps(obj, separators=(",", ":")) + "\n").encode("utf-8")
        self._sock.sendall(data)

    def _call(self, op: str, args: Optional[dict] = None,
              token: Optional[str] = None, timeout: float = 15.0) -> dict:
        # client timeout (15s) is strictly > server reply wait (8s) so the
        # server's reply — success OR synthesized TIMEOUT — always wins.
        with self._lock:
            self._seq += 1
            sid = self._seq
        q: "queue.Queue[dict]" = queue.Queue()
        with self._replies_lock:
            self._replies[sid] = q
        try:
            msg = {"v": PROTO, "id": sid, "op": op, "args": args or {}}
            if token is not None:
                msg["token"] = token
            self._send_line(msg)
            try:
                m = q.get(timeout=timeout)
            except queue.Empty:
                raise TimeoutError_(f"no response to '{op}' within {timeout}s")
            if m.get("__eof__"):
                raise ConnectError("connection closed while awaiting reply")
            # ids are guaranteed correct (per-id mailbox), but assert defensively
            if m.get("id") != sid:
                raise ProtocolError(f"reply id {m.get('id')} != request id {sid}")
            if not m.get("ok", False):
                err = m.get("err", {}) or {}
                raise _make_error(err.get("code", "ERR"), err.get("msg", ""))
            return m.get("result", {}) or {}
        finally:
            with self._replies_lock:
                self._replies.pop(sid, None)

    # ---- connection / run control ----
    def connect_virtual(self):
        self._call("connect_virtual")

    def connect_channels(self, channels: List[dict]):
        self._call("connect", {"channels": channels})

    def connect_device(self, device_type: str = "PCAN", *, sw_channel: int = 1,
                       channel_index: int = 0, device_index: int = 0,
                       fd: bool = False, baud: str = "500K", data_baud: str = "2M",
                       termination: bool = False, net_server: bool = False,
                       ip: str = "", port: str = "",
                       wait: bool = True, timeout: float = 3.0) -> bool:
        """Open ONE specified real CAN device by name.

        device_type is case-insensitive; recognized values include:
          "PCAN" (PEAK, classic CAN), "GCAN", "ZHCX",
          "USBCANFD-200U" / "USBCANFD-100U" / ... (ZLG, CAN FD),
          "USBCAN-E-U", "CANFDNET-TCP", "VIRTUAL".
        baud / data_baud are bitrate strings like "500K", "250K", "1M", "2M"
        (data_baud only matters when fd=True). channel_index selects the channel
        within the device (PCAN: 0->USBBUS1 ... 3->USBBUS4).

        All 11 channel fields are always sent. Returns True if the bus came up
        (when wait=True). Unlike the virtual bus, opening real hardware does NOT
        auto-fall-back: a missing card or driver DLL yields False here — check
        the app's log pane for the exact reason."""
        cfg = {
            "sw_channel": sw_channel, "is_fd": fd, "device_type": device_type,
            "device_index": device_index, "channel_index": channel_index,
            "baud": baud, "data_baud": data_baud, "termination": termination,
            "net_server": net_server, "ip": ip, "port": port,
        }
        self.connect_channels([cfg])
        if wait:
            return self.wait_connected(timeout=timeout)
        return True

    def connect_devices(self, devices, wait: bool = True, timeout: float = 5.0) -> bool:
        """Open MULTIPLE CAN cards/channels at once (real multi-card support).

        `devices` is a list of dev(...) dicts — give each a distinct sw_channel:
            pcan.connect_devices([
                pcanwork.dev("PCAN", sw_channel=1, channel_index=0, baud="500K"),
                pcanwork.dev("PCAN", sw_channel=2, channel_index=1, baud="250K"),
            ])
        Then route per card by the sw_channel you assigned:
            pcan.send(ch=1, id=0x100, data=...)         # card 1
            pcan.send(ch=2, id=0x200, data=...)         # card 2
            pcan.set_periodic(handle=1, ch=2, id=0x280, data=..., period_ms=100)
        and read back per card with last(ch=N, ...) / wait_for(ch=N, ...).
        Returns True if the bus(es) came up (when wait=True)."""
        self.connect_channels(list(devices))
        if wait:
            return self.wait_connected(timeout=timeout)
        return True

    def connect_configured(self, wait: bool = True, timeout: float = 5.0) -> int:
        """Open ALL channels configured in the app's 设备 (Device) dialog.

        Lets you set up several cards in the GUI (add/clone rows) and have the
        script open exactly those — handy for multi-card benches. Returns the
        number of configured channels (0 means none configured in the GUI)."""
        r = self._call("connect_configured")
        if wait:
            self.wait_connected(timeout=timeout)
        return int(r.get("channels", 0))

    def disconnect(self):
        self._call("disconnect")

    def start(self):
        self._call("start")

    def stop(self):
        self._call("stop")

    def status(self) -> dict:
        return self._call("status")

    def console_text(self) -> str:
        """Current CAN message-log (printf-over-CAN) text — the reassembled lines
        from frames the app is capturing (configure ID/channel in the 报文日志 tab).
        Lets a test assert on firmware printf output sent over CAN."""
        return self._call("console").get("text", "")

    def console_config(self, enabled: bool = None, id: int = None,
                       ch: int = None, clear: bool = False):
        """Configure the CAN message-log capture from a script.
        id=-1 means 'any ID', ch=0 means 'any channel'. Pass clear=True to wipe
        the current text. Any field left None is unchanged."""
        args = {"clear": clear}
        if enabled is not None:
            args["enabled"] = bool(enabled)
        if id is not None:
            args["id"] = int(id)
        if ch is not None:
            args["ch"] = int(ch)
        self._call("console_set", args)

    def wait_connected(self, timeout: float = 3.0) -> bool:
        """connect/start are fire-and-forget; poll status until the bus is up."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.status().get("connected"):
                return True
            time.sleep(0.05)
        return False

    # ---- send ----
    def send(self, ch: int, id: int, data, ext=False, fd=False, brs=False, remote=False):
        _check_ch(ch)
        self._call("send_once", {
            "ch": ch, "id": id, "data": list(bytes(data)),
            "ext": ext, "fd": fd, "brs": brs, "remote": remote})

    def send_signals(self, id: int, signals: Dict[str, float], ch: int = 1,
                     ext=False, fd=False):
        r = self._call("encode", {"id": id, "signals": signals})
        if not r.get("present"):
            raise RemoteError("NO_ENCODE", f"cannot encode 0x{id:X}")
        self.send(ch, id, bytes(r["data"]), ext=ext, fd=fd)

    def set_periodic(self, handle: int, ch: int, id: int, data, period_ms: int,
                     repeat: int = -1, ext=False, fd=False, brs=False, remote=False):
        _check_ch(ch)
        self._call("set_periodic", {
            "handle": handle, "ch": ch, "id": id, "data": list(bytes(data)),
            "period_ms": period_ms, "repeat": repeat,
            "ext": ext, "fd": fd, "brs": brs, "remote": remote})

    def stop_periodic(self, handle: int):
        self._call("stop_periodic", {"handle": handle})

    # ---- read / decode (served from app snapshot, no UI-tick wait) ----
    def last(self, ch: int, id: int, dir: str = "rx") -> Optional[Frame]:
        _check_ch(ch)
        r = self._call("get_last", {"ch": ch, "id": id, "dir": dir})
        if not r.get("present"):
            return None
        return Frame(ch=ch, id=id, data=bytes(r.get("data", [])),
                     t=r.get("t", 0.0), count=r.get("count", 0),
                     ext=r.get("ext", False), tx=(dir == "tx"))

    def decode(self, id: int, data) -> Dict[str, float]:
        r = self._call("decode", {"id": id, "data": list(bytes(data))})
        return {s["name"]: s["physical"] for s in r.get("signals", [])}

    def signal(self, ch: int, id: int, name: str, dir: str = "rx") -> Optional[float]:
        # dir is explicit: read back your own sent value with dir="tx".
        _check_ch(ch)
        r = self._call("get_signal", {"ch": ch, "id": id, "name": name, "dir": dir})
        return r.get("physical") if r.get("present") else None

    def load_dbc(self, path: str) -> str:
        """Load a .dbc file into the app so decode()/signal()/dbc_info() (and the
        main window) recognize its signals. `path` is on the machine running the
        app. Returns the loaded file name; raises RemoteError if it can't parse.
        Already-loaded files are a no-op. (You can also load DBCs from the main
        window's 加载DBC button.)"""
        r = self._call("load_dbc", {"path": path})
        return r.get("name", "")

    def dbc_info(self) -> List[dict]:
        """Every message+signal in the loaded DBC(s): a list of
        {id, name, dlc, file, signals:[{name, unit, min, max, ...}]}.
        Use it to DISCOVER the real signal names your DBC defines."""
        return self._call("dbc_info").get("messages", [])

    def signals_of(self, id: int) -> List[str]:
        """Signal names defined for message `id` in the loaded DBC(s)."""
        names: List[str] = []
        for m in self.dbc_info():
            if m.get("id") == id:
                names.extend(s["name"] for s in m.get("signals", []))
        return names

    # ---- await frame (the core test primitive) ----
    def subscribe(self, ids: Optional[List[int]] = None):
        # connect() already subscribed to all; call this to NARROW the stream.
        self._call("subscribe", {"ids": ids or []})
        self._subscribed_all = not ids

    def wait_for(self, ch: int, id: Optional[int],
                 predicate: Optional[Callable[[Frame], bool]] = None,
                 timeout: float = 2.0) -> Frame:
        """Block the SCRIPT (not the app) until a matching frame arrives.
        The connection is subscribed-to-all at handshake, so no race with a
        prior send()."""
        ids = {id} if id is not None else set()
        outq: "queue.Queue[Frame]" = queue.Queue()
        def match(f: Frame) -> bool:
            return (f.ch == ch) and (predicate(f) if predicate else True)
        token = (ids, match, outq)
        with self._evt_lock:
            self._evt_subs.append(token)
        try:
            deadline = time.monotonic() + timeout
            while True:
                remain = deadline - time.monotonic()
                if remain <= 0:
                    raise TimeoutError_(
                        f"no frame ch={ch} id="
                        f"{('0x%X' % id) if id is not None else 'any'} "
                        f"within {timeout}s")
                try:
                    return outq.get(timeout=remain)
                except queue.Empty:
                    continue
        finally:
            with self._evt_lock:
                self._evt_subs.remove(token)

    def wait_for_signal(self, ch: int, id: int, name: str,
                        cmp: Callable[[float], bool], timeout: float = 2.0) -> float:
        def pred(f: Frame) -> bool:
            d = self.decode(id, f.data)
            return name in d and cmp(d[name])
        f = self.wait_for(ch, id, predicate=pred, timeout=timeout)
        return self.decode(id, f.data)[name]

    # ---- assertions (in-process, no IPC) ----
    def expect(self, cond: bool, msg: str) -> bool:
        if cond:
            self.passed += 1
            self.log(f"PASS: {msg}")
        else:
            self.failed += 1
            self.log(f"FAIL: {msg}")
            print(f"[FAIL] {msg}", file=sys.stderr, flush=True)
        return bool(cond)

    def assert_eq(self, a, b, msg=""):
        return self.expect(a == b, f"{msg} ({a!r} == {b!r})")

    def assert_close(self, a, b, tol=1e-6, msg=""):
        return self.expect(abs(a - b) <= tol, f"{msg} (|{a}-{b}| <= {tol})")

    def assert_true(self, cond, msg=""):
        return self.expect(bool(cond), msg)

    # ---- misc ----
    def log(self, msg: str):
        try:
            self._call("log", {"msg": str(msg)})
        except PcanError:
            pass
        print(msg, flush=True)

    def sleep(self, s: float):
        time.sleep(s)

    def report(self) -> int:
        """Print summary, push run_result, return 0 if all passed else 1."""
        ok = self.failed == 0
        summary = f"{self.passed} passed, {self.failed} failed"
        if self._dropped:
            summary += f" ({self._dropped} frames dropped)"
        try:
            self._call("run_result", {"passed": ok, "summary": summary})
        except PcanError:
            pass
        print(("PASS — " if ok else "FAIL — ") + summary, flush=True)
        return 0 if ok else 1

    def close(self):
        try:
            self._sock.close()
        except OSError:
            pass

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        if exc_type is not None:           # surface an uncaught exception as a failed run
            self.failed += 1
            try:
                self._call("run_result", {"passed": False,
                                          "summary": f"exception: {exc}"})
            except PcanError:
                pass
        self.close()
        return False                       # do not swallow exceptions


def connect(host: str = "127.0.0.1",
            port: Optional[int] = None,
            token: Optional[str] = None,
            timeout: float = 5.0) -> Session:
    """Open a session, reading PCANWORK_IPC_PORT / PCANWORK_IPC_TOKEN from env
    when port/token are not given. Raises ConnectError on failure."""
    if port is None:
        ev = os.environ.get("PCANWORK_IPC_PORT")
        if not ev:
            raise ConnectError("PCANWORK_IPC_PORT not set — run this script "
                               "from PcanWork's Script Runner.")
        port = int(ev)
    if token is None:
        token = os.environ.get("PCANWORK_IPC_TOKEN", "")
    return Session(host, int(port), token, timeout=timeout)


def dev(device_type: str = "Virtual", *, sw_channel: int = 1, channel_index: int = 0,
        device_index: int = 0, fd: bool = False, baud: str = "500K",
        data_baud: str = "2M", termination: bool = False, net_server: bool = False,
        ip: str = "", port: str = "") -> dict:
    """Build ONE channel config (all 11 fields) for connect_devices().

    Give each card a distinct sw_channel — that is the number you later pass as
    send(ch=sw_channel, ...). channel_index selects the channel within the
    physical device; device_index selects which box of that type.

        pcanwork.dev("PCAN", sw_channel=1, channel_index=0, baud="500K")
        pcanwork.dev("USBCANFD-200U", sw_channel=2, channel_index=0, fd=True,
                     baud="500K", data_baud="2M")
    """
    return {
        "sw_channel": sw_channel, "is_fd": fd, "device_type": device_type,
        "device_index": device_index, "channel_index": channel_index,
        "baud": baud, "data_baud": data_baud, "termination": termination,
        "net_server": net_server, "ip": ip, "port": port,
    }


