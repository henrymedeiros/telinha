// Telinha server on the VPS: static site, PeerJS signaling at /peer
// and temporary coturn credentials at /api/ice.
import crypto from "node:crypto";
import express from "express";
import { ExpressPeerServer } from "peer";

const PORT = Number(process.env.PORT ?? 9000);
// Where the site's files are; the containers mount them at the default.
const PUBLIC_DIR = process.env.PUBLIC_DIR ?? "/srv/public";
const SECRET = process.env.TURN_SECRET;
const TURN_HOST = process.env.TURN_HOST;
const CRED_TTL = 24 * 3600;
// TURN over TLS (port 5349) needs coturn to have the site's certificate; the
// all-in-one setup (setup.sh) runs without it.
const TURN_TLS = process.env.TURN_TLS !== "off";
if (!SECRET || !TURN_HOST) throw new Error("set TURN_SECRET and TURN_HOST in .env");

const app = express();
app.disable("x-powered-by");
// How many reverse proxies sit in front of this server (Caddy: 1). The caller's
// address is then the X-Forwarded-For entry that proxy wrote, which a caller
// cannot forge. Behind one more proxy or a CDN this must be 2, or every visitor
// counts as one.
app.set("trust proxy", Number(process.env.TRUST_PROXY ?? 1));
// Only the site itself (and a local copy for development) may call this server
// from a browser: other sites cannot hand out this server's TURN credentials.
// The desktop app is not a browser, so this does not affect it.
const SITE = `https://${TURN_HOST}`;
const LOCAL = /^http:\/\/(localhost|127\.0\.0\.1)(:\d+)?$/;
app.use((req, res, next) => {
  const origin = req.get("Origin");
  if (origin === SITE || LOCAL.test(origin ?? "")) res.set("Access-Control-Allow-Origin", origin);
  res.set("Vary", "Origin");
  next();
});
// What the page may load and where it may connect. Scripts only from this
// site (PeerJS is served here too). Styles also inline (the download page's
// own block) and from Google Fonts, with its font files. Pictures only from
// here or drawn in the page (data:). Connections to this server and to the
// public PeerJS server the page falls back to. Nobody may frame the site.
const CSP = [
  "default-src 'self'",
  "script-src 'self'",
  "style-src 'self' 'unsafe-inline' https://fonts.googleapis.com",
  "font-src https://fonts.gstatic.com",
  "img-src 'self' data:",
  `connect-src 'self' wss://${TURN_HOST} https://0.peerjs.com wss://0.peerjs.com`,
  "object-src 'none'",
  "base-uri 'self'",
  "form-action 'self'",
  "frame-ancestors 'none'",
].join("; ");
app.use((req, res, next) => {
  res.set({ "Content-Security-Policy": CSP, "X-Content-Type-Options": "nosniff", "Referrer-Policy": "no-referrer", "X-Frame-Options": "DENY" });
  next();
});

// At most ICE_LIMIT relay credentials per address every ten minutes: a page
// asks once per load and the desktop app once per channel, so real groups stay
// far below it, and a script cannot mint them without bound. One credential
// still works for its whole lifetime; this only bounds how many exist.
const ICE_LIMIT = Number(process.env.ICE_LIMIT ?? 60);
const ICE_WINDOW = 10 * 60_000;
const ICE_ADDRESSES = 20_000; // the table never grows past this
const iceCalls = new Map(); // address -> { count, until }

// IPv6 callers count by /64 (one home or one machine), IPv4 by address.
function callerKey(ip = "") {
  if (ip.startsWith("::ffff:")) return ip.slice(7);
  if (!ip.includes(":")) return ip;
  const [head, tail = ""] = ip.split("::");
  const h = head ? head.split(":") : [], t = tail ? tail.split(":") : [];
  const groups = ip.includes("::") ? [...h, ...Array(8 - h.length - t.length).fill("0"), ...t] : h;
  return groups.slice(0, 4).join(":");
}

// 0 when this call is allowed, otherwise the seconds until it will be.
function iceWait(key, now = Date.now()) {
  let calls = iceCalls.get(key);
  if (!calls || now >= calls.until) {
    iceCalls.delete(key);
    if (iceCalls.size >= ICE_ADDRESSES) {
      for (const [k, c] of iceCalls) if (now >= c.until) iceCalls.delete(k);
      if (iceCalls.size >= ICE_ADDRESSES) iceCalls.delete(iceCalls.keys().next().value); // the oldest
    }
    calls = { count: 0, until: now + ICE_WINDOW };
    iceCalls.set(key, calls);
  }
  calls.count++;
  // One line per address per window, so a wrong TRUST_PROXY (everyone counted
  // as the proxy) shows up in the log.
  if (calls.count === ICE_LIMIT + 1) console.warn(`ice: limit reached for ${key}`);
  return calls.count <= ICE_LIMIT ? 0 : Math.ceil((calls.until - now) / 1000);
}

// Credential in coturn's `use-auth-secret` format: username = expiry in
// seconds, password = HMAC-SHA1 of the username with the shared secret.
app.get("/api/ice", (req, res) => {
  res.set("Cache-Control", "no-store");
  const wait = iceWait(callerKey(req.ip));
  if (wait) return res.status(429).set("Retry-After", String(wait)).json({ erro: "muitos pedidos; tente de novo em instantes" });
  const username = `${Math.floor(Date.now() / 1000) + CRED_TTL}:telinha`;
  const credential = crypto.createHmac("sha1", SECRET).update(username).digest("base64");
  res.json({
    iceServers: [
      { urls: `stun:${TURN_HOST}:3478` },
      {
        urls: [
          `turn:${TURN_HOST}:3478?transport=udp`,
          `turn:${TURN_HOST}:3478?transport=tcp`,
          ...(TURN_TLS ? [`turns:${TURN_HOST}:5349?transport=tcp`] : []),
        ],
        username,
        credential,
      },
    ],
  });
});

const server = app.listen(PORT, () => console.log(`telinha on port ${PORT}`));
app.use("/peer", ExpressPeerServer(server, { path: "/", proxied: true, alive_timeout: 60000 }));
app.use(express.static(PUBLIC_DIR, {
  setHeaders: (res) => res.set("Cache-Control", "no-cache"),
}));
