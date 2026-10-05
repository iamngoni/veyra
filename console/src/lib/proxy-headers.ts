/**
 * Header rules for the Worker's `/api/*` proxy.
 *
 * Boundary: the proxy calls the control API with a Cloudflare Access service
 * token, and Access answers such calls with `Set-Cookie: CF_Authorization=…`
 * (a service-token JWT). That cookie is the proxy's credential, not the
 * visitor's. Relayed to the browser it replaces the visitor's own Access
 * cookie, after which every background request is rejected as
 * unauthenticated and redirected to the login host, which the browser reports
 * as a CORS error. Nothing in this module may let an upstream cookie through.
 */

/** Response headers that belong to the proxy-to-API hop and must not reach the browser. */
const UPSTREAM_ONLY_RESPONSE_HEADERS = ['set-cookie', 'set-cookie2', 'cf-access-jwt-assertion'] as const

/**
 * Copies upstream response headers for the browser, dropping every cookie and
 * Access credential the API's gateway attached for the proxy.
 */
export function browserSafeResponseHeaders(upstream: Headers): Headers {
  const safe = new Headers(upstream)
  for (const name of UPSTREAM_ONLY_RESPONSE_HEADERS) {
    safe.delete(name)
  }
  return safe
}
