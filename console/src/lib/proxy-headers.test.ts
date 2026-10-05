/**
 * Pins the rule that the API proxy never relays upstream cookies: a leaked
 * service-token `CF_Authorization` cookie overwrote the visitor's own Access
 * cookie and broke every background request.
 */

import { describe, expect, it } from 'vitest'

import { browserSafeResponseHeaders } from '#/lib/proxy-headers'

describe('browserSafeResponseHeaders', () => {
  it('drops the service-token Access cookie but keeps content headers', () => {
    const upstream = new Headers({ 'content-type': 'application/json', 'cache-control': 'no-store' })
    upstream.append('set-cookie', 'CF_Authorization=service-token-jwt; Path=/; HttpOnly')
    upstream.append('set-cookie', 'CF_AppSession=abc; Path=/')

    const safe = browserSafeResponseHeaders(upstream)

    expect(safe.get('set-cookie')).toBeNull()
    expect(safe.getSetCookie()).toEqual([])
    expect(safe.get('content-type')).toBe('application/json')
    expect(safe.get('cache-control')).toBe('no-store')
  })

  it('drops an Access assertion header and leaves the input untouched', () => {
    const upstream = new Headers({ 'cf-access-jwt-assertion': 'jwt', etag: '"v1"' })

    const safe = browserSafeResponseHeaders(upstream)

    expect(safe.get('cf-access-jwt-assertion')).toBeNull()
    expect(safe.get('etag')).toBe('"v1"')
    expect(upstream.get('cf-access-jwt-assertion')).toBe('jwt')
  })

  it('passes headers through unchanged when no cookie is present', () => {
    const upstream = new Headers({ 'content-type': 'text/event-stream' })

    expect([...browserSafeResponseHeaders(upstream).entries()]).toEqual([...upstream.entries()])
  })
})
