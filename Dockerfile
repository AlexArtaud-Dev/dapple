# The dapple image: the static binary and nothing else. Built by CI from the release binaries
# (dist/dapple-linux-<arch>); it runs the discovery relay unless told otherwise.
FROM scratch
ARG TARGETARCH
COPY --chmod=0755 dist/dapple-linux-${TARGETARCH} /dapple
USER 65532:65532
EXPOSE 8691
ENTRYPOINT ["/dapple"]
CMD ["relay"]
