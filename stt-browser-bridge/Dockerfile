# STT Browser Bridge: Linux + PulseAudio + Xvfb for Web Speech API in headless CI.
# Host runs: docker run -p 8765:8765 -p 8766:8766 stt-browser-bridge
# Then: LOCALGPT_STT_WS_ENDPOINT=ws://127.0.0.1:8765 cargo test --features voice stt_browser_bridge_accepts_fixture -- --include-ignored --nocapture

FROM node:20-bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    chromium \
    pulseaudio \
    xvfb \
    ca-certificates \
    fonts-liberation \
    libasound2 \
    libatk-bridge2.0-0 \
    libatk1.0-0 \
    libcups2 \
    libdbus-1-3 \
    libdrm2 \
    libgbm1 \
    libgtk-3-0 \
    libnspr4 \
    libnss3 \
    libxcomposite1 \
    libxdamage1 \
    libxfixes3 \
    libxkbcommon0 \
    libxrandr2 \
    && rm -rf /var/lib/apt/lists/*

ENV PUPPETEER_SKIP_CHROMIUM_DOWNLOAD=true
ENV PUPPETEER_EXECUTABLE_PATH=/usr/bin/chromium

WORKDIR /app
COPY package.json package-lock.json* ./
RUN npm ci --omit=dev 2>/dev/null || npm install --omit=dev
COPY server.js virtual-device.js inject.js ./

RUN useradd -m -s /bin/sh bridge && chown -R bridge:bridge /app

COPY docker-entrypoint.sh /docker-entrypoint.sh
RUN chmod 755 /docker-entrypoint.sh

EXPOSE 8765 8766
USER bridge
ENTRYPOINT ["/docker-entrypoint.sh"]
