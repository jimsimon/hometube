FROM node:24-alpine

WORKDIR /app

# Install deps + copy sources as the `node` user (UID 1000, baked into
# the official image) so on-disk ownership matches the runtime user.
# Compose deployments may override `user:` to a different UID (e.g.
# 568 on TrueNAS); world-readable defaults keep that working.
COPY --chown=node:node package.json package-lock.json ./
RUN npm ci --omit=dev

COPY --chown=node:node server.js ./

USER node

EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD wget -qO- http://localhost:3000/health || exit 1

CMD ["node", "server.js"]
