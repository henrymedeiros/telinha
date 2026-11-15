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

// Credential in coturn's `use-auth-secret` format: username = expiry in
// seconds, password = HMAC-SHA1 of the username with the shared secret.
app.get("/api/ice", (req, res) => {
  const username = `${Math.floor(Date.now() / 1000) + CRED_TTL}:telinha`;
  const credential = crypto.createHmac("sha1", SECRET).update(username).digest("base64");
  res.set("Cache-Control", "no-store");
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
