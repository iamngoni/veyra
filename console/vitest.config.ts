/**
 * Standalone Vitest configuration for the console.
 *
 * The app ships through the TanStack Start plugin, which is not part of the
 * test runtime; tests render the exported components and the dashboard
 * directly, so only React JSX support is needed here. Coverage is measured on
 * first-party source and gated at the repository-wide 95% standard; the only
 * exclusions are the generated route tree and Start-runtime glue.
 */

import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'

export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.{ts,tsx}'],
    coverage: {
      provider: 'v8',
      include: ['src/**/*.{ts,tsx}'],
      exclude: [
        'src/routeTree.gen.ts',
        'src/router.tsx',
        'src/routes/__root.tsx',
        'src/**/*.test.{ts,tsx}',
      ],
      thresholds: {
        lines: 95,
        functions: 95,
        branches: 95,
        statements: 95,
      },
    },
  },
})
