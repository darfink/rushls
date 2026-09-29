// A Cloudflare Worker in front of a Rushls origin with [playback.auth].
//
// A sample, not a packaged project: copy it into your own Worker and add the
// one dependency, `npm install jose`. It needs these variables:
//
//   ORIGIN        https://origin.example.com   (the Rushls HTTP(S) listener)
//   JWKS_URL      https://issuer.example.com/.well-known/jwks.json
//   ISSUER        must equal the token's `iss`
//   AUDIENCE      must equal the token's `aud`
//   STREAM_CLAIM  optional, default "stream"; the claim naming the stream
//
// Why a Worker at all: media bytes are the same for every viewer, so they
// should be cached once, without the token in the cache key. A plain cache
// rule that drops the token would then hand cached segments to anyone.
// This Worker verifies the token at the edge first, then serves media from
// a token-free cache entry. Playlists go to the origin unchanged: they are
// short-lived, blocking reloads must reach the origin, and a query-token
// playlist differs from a bearer one.
//
// It checks what Rushls checks: an RS256 or ES256 signature from the JWKS,
// `iss`, `aud`, `exp`, `nbf`, and that the stream claim equals the requested
// stream. Add any extra claims you configure under [playback.auth.claims].

import { createRemoteJWKSet, jwtVerify } from "jose";

// One key set per isolate, fetched and cached by jose. It refetches on an
// unknown `kid` at most every 30 s; Rushls itself does not, so a brand-new
// key can pass here and still be refused by the origin until it refreshes.
let jwks;

export default {
  async fetch(request, env) {
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response(null, { status: 405 });
    }
    const url = new URL(request.url);
    const token = bearer(request) ?? url.searchParams.get("token");
    if (!token) return new Response(null, { status: 401 });

    jwks ??= createRemoteJWKSet(new URL(env.JWKS_URL), { cacheMaxAge: 5 * 60 * 1000 });
    let claims;
    try {
      ({ payload: claims } = await jwtVerify(token, jwks, {
        issuer: env.ISSUER,
        audience: env.AUDIENCE,
        algorithms: ["RS256", "ES256"],
        requiredClaims: ["exp"],
        clockTolerance: 30, // Rushls's default playback.auth.leeway
      }));
    } catch {
      return new Response(null, { status: 401 });
    }
    const stream = requestedStream(url.pathname);
    if (stream !== null && claims[env.STREAM_CLAIM ?? "stream"] !== stream) {
      return new Response(null, { status: 403 });
    }

    // The origin checks the token again, so it is forwarded as presented.
    const upstream = new Request(new URL(url.pathname + url.search, env.ORIGIN), request);
    if (!isMedia(url.pathname)) return fetch(upstream);

    const key = new URL(upstream.url);
    key.searchParams.delete("token");
    const cache = caches.default;
    // `match` answers a Range request from a cached full response.
    const hit = await cache.match(new Request(key, { headers: rangeOnly(request) }));
    if (hit) return hit;

    // Fetch the whole object: a 206 cannot be cached.
    const whole = new Headers(upstream.headers);
    whole.delete("range");
    const response = await fetch(new Request(upstream, { headers: whole }));
    if (!response.ok) return response;
    await cache.put(key, response.clone());
    if (!request.headers.has("range")) return response;
    return (await cache.match(new Request(key, { headers: rangeOnly(request) }))) ?? response;
  },
};

function bearer(request) {
  const match = request.headers.get("authorization")?.match(/^Bearer\s+(\S+)$/i);
  return match ? match[1] : null;
}

function rangeOnly(request) {
  const range = request.headers.get("range");
  return range ? { range } : {};
}

// Media paths are /{stream}/{rendition}/{init|segment|part}/{id}.{ext}.
function isMedia(pathname) {
  return /\/\d+\/(init|segment|part)\/\d+\.(mp4|m4s|vtt)$/.test(pathname);
}

// The stream is everything before the rendition-specific or playlist tail,
// mirroring the origin's own path parsing. A stream ID may contain slashes.
function requestedStream(pathname) {
  const parts = pathname.split("/").filter(Boolean);
  let tail;
  if (isMedia(pathname)) tail = 3;
  else if (parts.at(-1) === "index.m3u8") tail = 1;
  else if (parts.at(-1)?.endsWith(".m3u8") && /^\d+$/.test(parts.at(-2) ?? "")) tail = 2;
  else return null;
  if (parts.length <= tail) return null;
  return parts.slice(0, -tail).map(decodeURIComponent).join("/");
}
