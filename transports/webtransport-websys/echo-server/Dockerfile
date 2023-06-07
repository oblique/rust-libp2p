# syntax=docker/dockerfile:1.5-labs

FROM docker.io/golang:1.20 AS builder
WORKDIR /workspace
ADD . .
RUN go build .

FROM scratch
COPY --from=builder /workspace/echo-server /
