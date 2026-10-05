import { defineConfig } from 'vite'
import { devtools } from '@tanstack/devtools-vite'

import { tanstackStart } from '@tanstack/react-start/plugin/vite'

import viteReact from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

// The console talks to the Veyra control surface through the dev server so the
// browser never needs cross-origin access to the loopback service. Point the
// proxy at another host (for example a tunnel or a future always-on machine)
// by changing the target here.
const SERVICE = process.env.VEYRA_API_TARGET ?? 'http://127.0.0.1:8080'

// Extra host names the preview server accepts, for example the machine's
// Tailscale name when the console is reached through `tailscale serve`.
// Comma-separated; empty keeps Vite's localhost-only default.
const ALLOWED_HOSTS = (process.env.VEYRA_CONSOLE_ALLOWED_HOSTS ?? '')
  .split(',')
  .map((host) => host.trim())
  .filter(Boolean)

const config = defineConfig({
  resolve: { tsconfigPaths: true },
  plugins: [devtools(), tailwindcss(), tanstackStart(), viteReact()],
  server: {
    allowedHosts: ALLOWED_HOSTS,
    proxy: {
      '/api': {
        target: SERVICE,
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/api/, ''),
      },
    },
  },
  preview: {
    allowedHosts: ALLOWED_HOSTS,
    proxy: {
      '/api': {
        target: SERVICE,
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/api/, ''),
      },
    },
  },
})

export default config
