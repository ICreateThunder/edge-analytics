FROM gcr.io/distroless/cc-debian12:nonroot

COPY edge-analytics-binary /analytics

ENV PORT=3001
EXPOSE 3001

ENTRYPOINT ["/analytics"]
