/** Minimal typing for the Workers runtime module used by the `/api` proxy route. */
declare module 'cloudflare:workers' {
  export const env: Record<string, unknown>
}
