# -*- coding: utf-8 -*-
"""本地验证用: 对 Slint 内嵌 MCP server 发 JSON-RPC。用法:
   python zz_mcp.py <method> '<params-json>'
截图工具 take_screenshot 会把返回的 PNG 存到 zz_shot.png。"""
import sys, json, base64, urllib.request

PORT = 8090
URL = f"http://127.0.0.1:{PORT}/mcp"
_id = [0]


def call(method, params=None):
    _id[0] += 1
    body = {"jsonrpc": "2.0", "id": _id[0], "method": method}
    if params is not None:
        body["params"] = params
    req = urllib.request.Request(
        URL, data=json.dumps(body).encode("utf-8"),
        headers={"Content-Type": "application/json",
                 "Accept": "application/json, text/event-stream"},
        method="POST")
    with urllib.request.urlopen(req, timeout=15) as r:
        raw = r.read().decode("utf-8", "replace")
    # 可能是 SSE(data: ...) 或纯 JSON
    if raw.lstrip().startswith("event:") or "\ndata:" in raw or raw.startswith("data:"):
        for line in raw.splitlines():
            if line.startswith("data:"):
                raw = line[5:].strip()
                break
    return json.loads(raw)


def tool(name, args=None):
    return call("tools/call", {"name": name, "arguments": args or {}})


if __name__ == "__main__":
    method = sys.argv[1]
    params = json.loads(sys.argv[2]) if len(sys.argv) > 2 else None
    # 便捷: 直接写工具名 → 当 tools/call
    known_tools = {"list_windows", "get_window_properties", "get_element_tree",
                   "query_element_descendants", "find_elements_by_id",
                   "get_element_properties", "take_screenshot", "click_element",
                   "set_element_value", "dispatch_key_event", "invoke_accessibility_action"}
    if method in known_tools:
        resp = tool(method, params)
    else:
        resp = call(method, params)

    # 截图: 抽出 image content 存 PNG
    res = resp.get("result", {})
    contents = res.get("content", []) if isinstance(res, dict) else []
    for c in contents:
        if c.get("type") == "image" and c.get("data"):
            open("zz_shot.png", "wb").write(base64.b64decode(c["data"]))
            c["data"] = f"<{len(c['data'])} b64 chars -> zz_shot.png>"
    print(json.dumps(resp, ensure_ascii=False, indent=1)[:6000])
