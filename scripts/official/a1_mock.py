# A1 mock: 只记录头名/存在性/布尔值，绝不记录 Authorization、x-api-key 的值
import json, os, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = sys.argv[2]
PORT = int(sys.argv[1])
READY_TOKEN = os.environ.get("A1_READY_TOKEN", "a1-mock-ready").encode()
LOG_LOCK = threading.Lock()


def record(h, method, path):
    names = sorted({k.lower() for k in h.keys()})
    auth = h.get("authorization") or ""
    beta = h.get("anthropic-beta") or ""
    ua = (h.get("user-agent") or "").split(" ")[0]
    entry = {
        "t": time.strftime("%H:%M:%S"),
        "method": method,
        "path": path,
        "header_names": names,
        "authorization_present": bool(auth.strip()),
        "authorization_is_bearer": auth.lower().startswith("bearer "),
        "authorization_looks_oauth": auth[7:].startswith("sk-ant-oat") if auth.lower().startswith("bearer ") else False,
        "x_api_key_present": "x-api-key" in names,
        "anthropic_beta_present": bool(beta),
        "anthropic_beta_has_oauth": "oauth" in beta.lower(),
        "user_agent_prefix": ua,
    }
    return entry


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _log(self, method):
        e = record(self.headers, method, self.path.split("?")[0])
        with LOG_LOCK:
            with open(LOG, "a") as f:
                f.write(json.dumps(e, ensure_ascii=False) + "\n")

    def _read_body(self):
        te = (self.headers.get("transfer-encoding") or "").lower()
        if "chunked" in te:
            chunks = []
            while True:
                line = self.rfile.readline()
                if not line:
                    break
                size = int(line.split(b";", 1)[0].strip(), 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.read(2)
            return b"".join(chunks)
        n = int(self.headers.get("content-length") or 0)
        return self.rfile.read(n) if n else b""

    def do_GET(self):
        if self.path == "/__a1_ready":
            body = READY_TOKEN
            self.send_response(200)
            self.send_header("content-type", "text/plain")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self._log("GET")
        body = b'{"data":[],"has_more":false}'
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_HEAD(self):
        self._log("HEAD")
        self.send_response(200)
        self.end_headers()

    def do_POST(self):
        raw = self._read_body()
        self._log("POST")
        try:
            req = json.loads(raw or b"{}")
        except Exception:
            req = {}
        model = req.get("model", "mock")
        if self.path.startswith("/v1/messages/count_tokens"):
            body = b'{"input_tokens":1}'
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        msg = {"id": "msg_mock", "type": "message", "role": "assistant", "model": model,
               "content": [], "stop_reason": None, "stop_sequence": None,
               "usage": {"input_tokens": 1, "output_tokens": 1}}
        if req.get("stream"):
            evs = [
                ("message_start", {"type": "message_start", "message": msg}),
                ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "mock-ok"}}),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 1}}),
                ("message_stop", {"type": "message_stop"}),
            ]
            body = "".join(f"event: {e}\ndata: {json.dumps(d)}\n\n" for e, d in evs).encode()
            ctype = "text/event-stream"
        else:
            msg["content"] = [{"type": "text", "text": "mock-ok"}]
            msg["stop_reason"] = "end_turn"
            body = json.dumps(msg).encode()
            ctype = "application/json"
        self.send_response(200)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
