FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
RUN useradd -r -s /usr/sbin/nologin analytics

COPY edge-analytics-binary /usr/local/bin/analytics

USER analytics
ENV PORT=3001
EXPOSE 3001

ENTRYPOINT ["analytics"]
