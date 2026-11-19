# Build context: repository root (the site bundles docs/ and integrations/ at build time).
#   docker build -f site/Dockerfile -t space3d-site .
FROM node:26-slim AS build
RUN npm install -g pnpm@10.33.2
WORKDIR /repo
COPY README.md ./
COPY docs ./docs
COPY integrations ./integrations
COPY site ./site
WORKDIR /repo/site
RUN pnpm install --frozen-lockfile && pnpm run build

FROM nginxinc/nginx-unprivileged:stable-alpine
COPY site/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=build /repo/site/dist /usr/share/nginx/html
EXPOSE 8080
HEALTHCHECK CMD wget -qO- http://127.0.0.1:8080/ >/dev/null || exit 1
