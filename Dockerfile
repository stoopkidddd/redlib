FROM alpine:3.20.0

RUN apk add --no-cache wget

RUN wget https://github.com/redlib-org/redlib/releases/latest/download/redlib && \
    mv redlib /usr/local/bin/ && \
    chmod +x /usr/local/bin/redlib

RUN adduser --home /nonexistent --no-create-home --disabled-password redlib
USER redlib

# Tell Docker to expose port 8080
EXPOSE 8080

# Run a healthcheck every minute to make sure redlib is functional
HEALTHCHECK --interval=1m --timeout=3s CMD wget --spider --q http://localhost:8080/settings || exit 1

CMD ["redlib"]
