FROM gcr.io/distroless/cc-debian13:nonroot

COPY edge-analytics-binary /analytics

ENV PORT=3001
EXPOSE 3001

ENTRYPOINT ["/analytics"]
