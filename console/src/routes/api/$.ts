/**
 * Worker-side replacement for the Vite dev proxy: forwards `/api/*` to the
 * Veyra control surface over the private tunnel hostname.
 *
 * Boundary: the origin is read from the `VEYRA_API_ORIGIN` binding and the
 * Cloudflare Access service-token pair from secrets. Nothing is defaulted; a
 * missing binding is a 503, never a silent fallback. Only an allow-list of
 * request headers is forwarded, so a browser can never pick its own identity
 * toward the origin, and no upstream cookie is ever relayed back (see
 * `proxy-headers.ts`).
 */
import { createFileRoute } from '@tanstack/react-router'
import { env } from 'cloudflare:workers'

import { browserSafeResponseHeaders } from '#/lib/proxy-headers'

interface ProxyEnv {
  VEYRA_API_ORIGIN?: string
  CF_ACCESS_CLIENT_ID?: string
  CF_ACCESS_CLIENT_SECRET?: string
}

/** Request headers the control API needs; everything else is deliberately not forwarded. */
const FORWARDED_REQUEST_HEADERS = [
  'accept',
  'accept-language',
  'content-type',
  'if-none-match',
  'last-event-id',
  'x-veyra-admin-token',
]

/** An error body the console can show; never a bare redirect the browser would call CORS. */
function failure(error: string, status: number, detail?: string): Response {
  return Response.json({ error, detail }, { status, headers: { 'cache-control': 'no-store' } })
}

async function forward({ request }: { request: Request }): Promise<Response> {
  const { VEYRA_API_ORIGIN, CF_ACCESS_CLIENT_ID, CF_ACCESS_CLIENT_SECRET } = env as ProxyEnv
  if (!VEYRA_API_ORIGIN || !CF_ACCESS_CLIENT_ID || !CF_ACCESS_CLIENT_SECRET) {
    return failure('api_origin_not_configured', 503)
  }
  const incoming = new URL(request.url)
  const target = new URL(incoming.pathname.replace(/^\/api/, '') + incoming.search, VEYRA_API_ORIGIN)

  const headers = new Headers()
  for (const name of FORWARDED_REQUEST_HEADERS) {
    const value = request.headers.get(name)
    if (value !== null) headers.set(name, value)
  }
  headers.set('CF-Access-Client-Id', CF_ACCESS_CLIENT_ID)
  headers.set('CF-Access-Client-Secret', CF_ACCESS_CLIENT_SECRET)

  let upstream: Response
  try {
    upstream = await fetch(target, {
      method: request.method,
      headers,
      body: ['GET', 'HEAD'].includes(request.method) ? undefined : request.body,
      redirect: 'manual',
    })
  } catch (error) {
    console.error('veyra api unreachable', target.pathname, String(error))
    return failure('api_unreachable', 502, String(error))
  }

  // A redirect here means Access (or the tunnel) did not accept the service
  // token. Passing it through would make the browser follow it cross-origin
  // and report a CORS error that hides the real cause.
  if (upstream.status >= 300 && upstream.status < 400) {
    console.error('veyra api redirected', target.pathname, upstream.status)
    return failure('api_access_denied', 502, `upstream answered ${upstream.status}`)
  }
  if (upstream.status >= 500) {
    console.error('veyra api error', target.pathname, upstream.status)
  }
  // Never relay the API gateway's cookies: they carry the proxy's service-token
  // identity and would replace the visitor's own Access cookie.
  return new Response(upstream.body, {
    status: upstream.status,
    statusText: upstream.statusText,
    headers: browserSafeResponseHeaders(upstream.headers),
  })
}

export const Route = createFileRoute('/api/$')({
  server: {
    handlers: {
      GET: forward,
      POST: forward,
      PUT: forward,
      DELETE: forward,
    },
  },
})
