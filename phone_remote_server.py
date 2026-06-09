#!/usr/bin/env python3
"""Small authenticated web remote for iPhone Mirroring.

Serves http://127.0.0.1:8787/phone with live screenshots and tap/swipe controls.
Designed to sit behind a temporary Cloudflare Tunnel.
"""
from __future__ import annotations

import base64
import functools
import hashlib
import hmac
import html
import http.cookies
import json
import os
import secrets
import shlex
import subprocess
import tempfile
import threading
import time
import urllib.parse
from http import HTTPStatus
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
from pathlib import Path

HOST = os.environ.get("PHONE_REMOTE_HOST", "127.0.0.1")
PORT = int(os.environ.get("PHONE_REMOTE_PORT", "8787"))
PASSWORD = os.environ.get("PHONE_REMOTE_PASSWORD") or secrets.token_urlsafe(12)
SECRET = os.environ.get("PHONE_REMOTE_SECRET") or secrets.token_urlsafe(32)
SESSION_TTL = int(os.environ.get("PHONE_REMOTE_SESSION_TTL", str(8 * 3600)))
IPHONE_SHOT = os.environ.get("IPHONE_SHOT", "/Users/leo/bin/iphone-shot")
IPHONE_ACT = os.environ.get("IPHONE_ACT", "/Users/leo/bin/iphone-act")
CUA = os.environ.get("CUA_DRIVER", "/Users/leo/.local/bin/cua-driver")
TMP_DIR = Path(os.environ.get("PHONE_REMOTE_TMP", "/tmp/hermes-phone-remote"))
TMP_DIR.mkdir(parents=True, exist_ok=True)

screenshot_lock = threading.Lock()
action_lock = threading.Lock()
last_screenshot = TMP_DIR / "latest.png"

PHONE_HTML = r"""
<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover" />
  <title>iPhone Remote</title>
  <style>
    :root { color-scheme: dark; --bg:#08090c; --panel:#11131a; --line:#272b38; --text:#eef2ff; --muted:#9ca3af; --accent:#4f8cff; --danger:#ff5a66; }
    * { box-sizing: border-box; }
    body { margin:0; font-family: -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; background:var(--bg); color:var(--text); }
    header { position:sticky; top:0; z-index:10; display:flex; gap:8px; align-items:center; flex-wrap:wrap; padding:10px; background:rgba(8,9,12,.88); backdrop-filter: blur(10px); border-bottom:1px solid var(--line); }
    button, input { border:1px solid var(--line); background:var(--panel); color:var(--text); border-radius:10px; padding:9px 11px; font-size:14px; }
    button { cursor:pointer; user-select:none; }
    button.primary { background:var(--accent); border-color:var(--accent); color:white; }
    button.danger { background:#2a1116; border-color:#5b1b24; color:#ffb8bf; }
    input { min-width: 150px; }
    #status { color:var(--muted); font-size:13px; margin-left:auto; }
    main { display:flex; justify-content:center; padding:10px; }
    #phoneWrap { position:relative; width:min(100%, 460px); touch-action:none; border-radius:28px; overflow:hidden; box-shadow:0 18px 60px rgba(0,0,0,.55); background:#000; }
    #phone { display:block; width:100%; height:auto; user-select:none; -webkit-user-drag:none; }
    #tapDot { position:absolute; width:24px; height:24px; border:2px solid #fff; background:rgba(79,140,255,.45); border-radius:50%; transform:translate(-50%,-50%); pointer-events:none; opacity:0; transition:opacity .25s; }
    #grid { position:absolute; inset:0; pointer-events:none; display:none; }
    #grid.on { display:block; }
    .hint { color:var(--muted); font-size:12px; padding: 0 12px 12px; text-align:center; }
    .toast { position:fixed; left:50%; bottom:18px; transform:translateX(-50%); background:#111827; border:1px solid var(--line); color:var(--text); padding:10px 12px; border-radius:12px; opacity:0; transition:.2s; z-index:99; max-width:90vw; }
    .toast.on { opacity:1; }
  </style>
</head>
<body>
<header>
  <button class="primary" id="refresh">刷新</button>
  <button id="auto">自动: 开</button>
  <button id="gridBtn">网格</button>
  <button id="home">Home</button>
  <button id="search">搜索</button>
  <button id="switcher">切换器</button>
  <input id="text" placeholder="输入到手机…" />
  <button id="typeBtn">输入</button>
  <button class="danger" id="logout">退出</button>
  <span id="status">ready</span>
</header>
<main>
  <div id="phoneWrap">
    <img id="phone" alt="iPhone screenshot" draggable="false" />
    <canvas id="grid"></canvas>
    <div id="tapDot"></div>
  </div>
</main>
<div class="hint">直接点截图=点击手机；短拖=滑动。公网 tunnel 已有登录保护，但仍建议只临时使用。</div>
<div id="toast" class="toast"></div>
<script>
const img = document.getElementById('phone');
const wrap = document.getElementById('phoneWrap');
const statusEl = document.getElementById('status');
const dot = document.getElementById('tapDot');
const grid = document.getElementById('grid');
let auto = true;
let busy = false;
let lastDown = null;
let timer = null;

function toast(msg){ const t=document.getElementById('toast'); t.textContent=msg; t.classList.add('on'); setTimeout(()=>t.classList.remove('on'), 1600); }
function setStatus(s){ statusEl.textContent=s; }
async function api(path, body){
  const r = await fetch(path, {method:'POST', headers:{'content-type':'application/json'}, body: JSON.stringify(body||{})});
  const j = await r.json().catch(()=>({ok:false,error:'bad json'}));
  if(!r.ok || !j.ok) throw new Error(j.error || r.statusText);
  return j;
}
function loadShot(){
  const ts = Date.now();
  setStatus('截图中…');
  img.onload = () => { setStatus(new Date().toLocaleTimeString()); drawGrid(); };
  img.onerror = () => setStatus('截图失败');
  img.src = '/api/screenshot?t=' + ts;
}
function clientToNatural(e){
  const rect = img.getBoundingClientRect();
  const sx = img.naturalWidth / rect.width;
  const sy = img.naturalHeight / rect.height;
  return {
    x: Math.round((e.clientX - rect.left) * sx),
    y: Math.round((e.clientY - rect.top) * sy),
    cx: e.clientX - rect.left,
    cy: e.clientY - rect.top
  };
}
function showDot(cx,cy){ dot.style.left=cx+'px'; dot.style.top=cy+'px'; dot.style.opacity='1'; setTimeout(()=>dot.style.opacity='0', 350); }
async function tapAt(p){
  if(busy) return; busy=true;
  showDot(p.cx,p.cy); setStatus(`tap ${p.x},${p.y}`);
  try { await api('/api/tap', {x:p.x, y:p.y}); toast(`点了 ${p.x},${p.y}`); setTimeout(loadShot, 350); }
  catch(e){ toast('点击失败: '+e.message); setStatus('error'); }
  finally { busy=false; }
}
async function swipeBy(dx,dy){
  if(busy) return; busy=true;
  const absx=Math.abs(dx), absy=Math.abs(dy);
  const direction = absx > absy ? (dx < 0 ? 'left':'right') : (dy < 0 ? 'up':'down');
  const dist = Math.max(absx,absy);
  const size = dist > 220 ? 'long' : (dist > 110 ? 'medium' : 'short');
  setStatus(`swipe ${direction}`);
  try { await api('/api/swipe', {direction, size}); toast(`滑动 ${direction}`); setTimeout(loadShot, 650); }
  catch(e){ toast('滑动失败: '+e.message); setStatus('error'); }
  finally { busy=false; }
}
wrap.addEventListener('pointerdown', e => { e.preventDefault(); wrap.setPointerCapture(e.pointerId); lastDown = {p:clientToNatural(e), t:Date.now()}; });
wrap.addEventListener('pointerup', e => {
  e.preventDefault(); if(!lastDown) return;
  const p = clientToNatural(e); const dx = p.cx-lastDown.p.cx, dy = p.cy-lastDown.p.cy;
  if(Math.hypot(dx,dy) < 18) tapAt(p); else swipeBy(dx,dy);
  lastDown = null;
});
function drawGrid(){
  if(!grid.classList.contains('on') || !img.naturalWidth) return;
  const rect=img.getBoundingClientRect();
  grid.width=rect.width*devicePixelRatio; grid.height=rect.height*devicePixelRatio;
  grid.style.width=rect.width+'px'; grid.style.height=rect.height+'px';
  const ctx=grid.getContext('2d'); ctx.scale(devicePixelRatio,devicePixelRatio);
  ctx.clearRect(0,0,rect.width,rect.height); ctx.strokeStyle='rgba(255,60,60,.55)'; ctx.lineWidth=1;
  const cols=12, rows=24;
  for(let c=0;c<=cols;c++){ const x=rect.width*c/cols; ctx.beginPath(); ctx.moveTo(x,0); ctx.lineTo(x,rect.height); ctx.stroke(); }
  for(let r=0;r<=rows;r++){ const y=rect.height*r/rows; ctx.beginPath(); ctx.moveTo(0,y); ctx.lineTo(rect.width,y); ctx.stroke(); }
  ctx.font='11px -apple-system'; ctx.fillStyle='rgba(255,80,80,.9)'; ctx.strokeStyle='rgba(255,255,255,.75)'; ctx.lineWidth=2;
  let n=1; for(let r=0;r<rows;r++){ for(let c=0;c<cols;c++){ const x=rect.width*c/cols+3, y=rect.height*r/rows+12; ctx.strokeText(String(n),x,y); ctx.fillText(String(n),x,y); n++; }}
}
document.getElementById('refresh').onclick=loadShot;
document.getElementById('auto').onclick=()=>{ auto=!auto; document.getElementById('auto').textContent='自动: '+(auto?'开':'关'); };
document.getElementById('gridBtn').onclick=()=>{ grid.classList.toggle('on'); drawGrid(); };
document.getElementById('home').onclick=async()=>{ await api('/api/shortcut',{name:'home'}); setTimeout(loadShot,600); };
document.getElementById('search').onclick=async()=>{ await api('/api/shortcut',{name:'spotlight'}); setTimeout(loadShot,600); };
document.getElementById('switcher').onclick=async()=>{ await api('/api/shortcut',{name:'switcher'}); setTimeout(loadShot,600); };
document.getElementById('typeBtn').onclick=async()=>{ const text=document.getElementById('text').value; if(!text) return; await api('/api/type',{text}); document.getElementById('text').value=''; setTimeout(loadShot,500); };
document.getElementById('text').addEventListener('keydown', e=>{ if(e.key==='Enter') document.getElementById('typeBtn').click(); });
document.getElementById('logout').onclick=()=>{ location.href='/logout'; };
window.addEventListener('resize', drawGrid);
timer=setInterval(()=>{ if(auto && !busy) loadShot(); }, 1200);
loadShot();
</script>
</body>
</html>
"""

LOGIN_HTML = r"""
<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Login</title><style>
body{margin:0;min-height:100vh;display:grid;place-items:center;background:#08090c;color:#eef2ff;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}.card{width:min(92vw,360px);background:#11131a;border:1px solid #272b38;border-radius:18px;padding:22px;box-shadow:0 18px 60px rgba(0,0,0,.5)}input,button{width:100%;padding:12px;border-radius:12px;border:1px solid #272b38;background:#08090c;color:#eef2ff;font-size:16px}button{margin-top:12px;background:#4f8cff;border-color:#4f8cff}.err{color:#ff9aa2;min-height:20px}.muted{color:#9ca3af;font-size:13px}</style></head><body><form class="card" method="post" action="/login"><h2>iPhone Remote</h2><p class="muted">请输入临时密码。建议只在需要时开启 tunnel。</p><p class="err">{error}</p><input name="password" type="password" autofocus autocomplete="current-password" placeholder="Password"><button>登录</button></form></body></html>
"""


def b64(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode().rstrip("=")


def sign(payload: str) -> str:
    return b64(hmac.new(SECRET.encode(), payload.encode(), hashlib.sha256).digest())


def make_token() -> str:
    exp = str(int(time.time()) + SESSION_TTL)
    nonce = secrets.token_urlsafe(12)
    payload = f"{exp}:{nonce}"
    return f"{payload}:{sign(payload)}"


def check_token(token: str) -> bool:
    try:
        exp, nonce, sig = token.split(":", 2)
        payload = f"{exp}:{nonce}"
        if int(exp) < time.time():
            return False
        return hmac.compare_digest(sign(payload), sig)
    except Exception:
        return False


def run(cmd, timeout=30):
    return subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=timeout)


def find_phone_window():
    data = json.loads(subprocess.check_output([CUA, "call", "list_windows", "{}"], text=True))
    cs = []
    for w in data.get("windows", []):
        app = (w.get("app_name") or "") + (w.get("title") or "")
        b = w.get("bounds") or {}
        ww = int(float(b.get("width", 0)))
        hh = int(float(b.get("height", 0)))
        if any(s in app for s in ["iPhone", "镜像", "Mirroring"]) and 200 <= ww <= 900 and 400 <= hh <= 1600:
            cs.append((1 if w.get("is_on_screen") else 0, ww * hh, w))
    if not cs:
        raise RuntimeError("没有找到 iPhone 镜像窗口")
    cs.sort(reverse=True, key=lambda x: (x[0], x[1]))
    return cs[0][2]


def cua_call(name, payload, timeout=20):
    p = run([CUA, "call", name, json.dumps(payload, ensure_ascii=False)], timeout=timeout)
    if p.returncode != 0:
        raise RuntimeError((p.stderr or p.stdout or f"{name} failed").strip())
    return p


class Handler(BaseHTTPRequestHandler):
    server_version = "PhoneRemote/1.0"

    def log_message(self, format, *args):
        print(f"[{time.strftime('%H:%M:%S')}] {self.client_address[0]} {format % args}", flush=True)

    def cookies(self):
        return http.cookies.SimpleCookie(self.headers.get("Cookie", ""))

    def authenticated(self):
        c = self.cookies().get("phone_session")
        return bool(c and check_token(c.value))

    def send_bytes(self, data: bytes, content_type="text/html; charset=utf-8", status=200, headers=None):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Frame-Options", "DENY")
        self.send_header("Referrer-Policy", "no-referrer")
        if headers:
            for k, v in headers.items():
                self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def redirect(self, path):
        self.send_response(302)
        self.send_header("Location", path)
        self.end_headers()

    def require_auth(self):
        if not self.authenticated():
            self.redirect("/login")
            return False
        return True

    def read_body(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        ctype = self.headers.get("Content-Type", "")
        if "application/json" in ctype:
            return json.loads(raw.decode() or "{}")
        return {k: v[0] for k, v in urllib.parse.parse_qs(raw.decode()).items()}

    def json_response(self, obj, status=200):
        self.send_bytes(json.dumps(obj, ensure_ascii=False).encode(), "application/json", status=status)

    def do_HEAD(self):
        path = urllib.parse.urlparse(self.path).path
        if path in {"/", "/login", "/phone"}:
            if path == "/phone" and not self.authenticated():
                self.send_response(302)
                self.send_header("Location", "/login")
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
        else:
            self.send_error(404)

    def do_GET(self):
        path = urllib.parse.urlparse(self.path).path
        if path == "/":
            self.redirect("/phone")
        elif path == "/login":
            self.send_bytes(LOGIN_HTML.replace("{error}", "").encode())
        elif path == "/logout":
            self.send_response(302)
            self.send_header("Location", "/login")
            self.send_header("Set-Cookie", "phone_session=; Max-Age=0; Path=/; HttpOnly; SameSite=Lax")
            self.end_headers()
        elif path == "/phone":
            if self.require_auth():
                self.send_bytes(PHONE_HTML.encode())
        elif path == "/api/screenshot":
            if not self.authenticated():
                self.send_bytes(b"unauthorized", "text/plain", status=401)
                return
            try:
                with screenshot_lock:
                    tmp = TMP_DIR / f"shot-{int(time.time()*1000)}.png"
                    p = run([IPHONE_SHOT, str(tmp)], timeout=25)
                    if p.returncode not in (0, 4):
                        raise RuntimeError((p.stderr or p.stdout or "screenshot failed").strip())
                    data = tmp.read_bytes()
                    last_screenshot.write_bytes(data)
                self.send_bytes(data, "image/png")
            except Exception as e:
                self.send_bytes(f"screenshot error: {e}".encode(), "text/plain", status=500)
        else:
            self.send_error(404)

    def do_POST(self):
        path = urllib.parse.urlparse(self.path).path
        if path == "/login":
            body = self.read_body()
            if hmac.compare_digest(body.get("password", ""), PASSWORD):
                cookie = f"phone_session={make_token()}; Path=/; HttpOnly; SameSite=Lax"
                # Cloudflare tunnel is HTTPS externally; browsers still accept non-Secure on localhost tests.
                self.send_response(302)
                self.send_header("Location", "/phone")
                self.send_header("Set-Cookie", cookie)
                self.end_headers()
            else:
                self.send_bytes(LOGIN_HTML.replace("{error}", html.escape("密码不对")).encode(), status=401)
            return
        if not self.authenticated():
            self.json_response({"ok": False, "error": "unauthorized"}, status=401)
            return
        try:
            body = self.read_body()
            with action_lock:
                if path == "/api/tap":
                    x = int(body["x"]); y = int(body["y"])
                    p = run([IPHONE_ACT, "tap", f"{x},{y}"], timeout=20)
                    if p.returncode != 0: raise RuntimeError((p.stderr or p.stdout).strip())
                    self.json_response({"ok": True, "out": p.stdout.strip(), "x": x, "y": y})
                elif path == "/api/swipe":
                    direction = body.get("direction")
                    size = body.get("size", "medium")
                    if direction not in {"up", "down", "left", "right"}:
                        raise ValueError("bad direction")
                    p = run([IPHONE_ACT, "swipe", direction, size], timeout=25)
                    if p.returncode != 0: raise RuntimeError((p.stderr or p.stdout).strip())
                    self.json_response({"ok": True, "out": p.stdout.strip()})
                elif path == "/api/shortcut":
                    name = body.get("name")
                    if name not in {"home", "spotlight", "switcher"}:
                        raise ValueError("bad shortcut")
                    p = run([IPHONE_ACT, name], timeout=20)
                    if p.returncode != 0: raise RuntimeError((p.stderr or p.stdout).strip())
                    self.json_response({"ok": True, "out": p.stdout.strip()})
                elif path == "/api/type":
                    text = str(body.get("text", ""))[:500]
                    w = find_phone_window(); pid = w["pid"]; wid = w["window_id"]
                    p = subprocess.run([CUA, "call", "type_text", json.dumps({"pid": pid, "window_id": wid, "text": text}, ensure_ascii=False)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=20)
                    if p.returncode != 0:
                        p = subprocess.run([CUA, "call", "type", json.dumps({"pid": pid, "window_id": wid, "text": text}, ensure_ascii=False)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=20)
                    if p.returncode != 0: raise RuntimeError((p.stderr or p.stdout).strip())
                    self.json_response({"ok": True})
                else:
                    self.json_response({"ok": False, "error": "not found"}, status=404)
        except Exception as e:
            self.json_response({"ok": False, "error": str(e)}, status=500)


def main():
    print("PHONE_REMOTE_PASSWORD=" + PASSWORD, flush=True)
    print(f"Serving http://{HOST}:{PORT}/phone", flush=True)
    httpd = ThreadingHTTPServer((HOST, PORT), Handler)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
