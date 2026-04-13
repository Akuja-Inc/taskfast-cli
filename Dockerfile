FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      jq \
 && rm -rf /var/lib/apt/lists/*

COPY target/release/taskfast /usr/local/bin/taskfast
COPY client-skills/taskfast-agent /opt/taskfast-skills

WORKDIR /work
CMD ["bash"]
