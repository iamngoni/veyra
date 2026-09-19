# syntax=docker/dockerfile:1.7
# Builds the operations console and serves its SSR bundle behind private ingress.

FROM node:22-alpine AS builder

WORKDIR /app
COPY package.json package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci

COPY . ./
RUN npm run build

FROM node:22-alpine AS runtime

WORKDIR /app
ENV NODE_ENV=production

COPY --from=builder --chown=node:node /app /app
RUN mkdir -p /app/node_modules/.vite-temp \
    && chown node:node /app/node_modules/.vite-temp

USER node
EXPOSE 3000
CMD ["./node_modules/.bin/vite", "preview", "--host", "0.0.0.0", "--port", "3000"]
