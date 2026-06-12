# Runner image for containerized stdio MCP servers (ghcr.io/endara-ai/mcp-runner).
#
# Ships Node LTS (`npx`) and uv (`uvx`) so endpoints can run arbitrary
# `npx -y <pkg>` / `uvx <pkg>` servers without any host-side installs.
FROM node:22-bookworm-slim

# uv / uvx, copied from the official multi-arch image (pinned).
COPY --from=ghcr.io/astral-sh/uv:0.7.13 /uv /uvx /usr/local/bin/

# ca-certificates for registry access; git for packages resolved from VCS refs.
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates git \
  && rm -rf /var/lib/apt/lists/*

# Run as the unprivileged `node` user (uid 1000) shipped with the base image.
USER node
WORKDIR /home/node
ENV NPM_CONFIG_UPDATE_NOTIFIER=false

# Pre-install a uv-managed CPython so `uvx` servers don't have to download an
# interpreter on every container start (`--rm` containers keep no cache).
RUN uv python install 3.12

CMD ["node", "--version"]
