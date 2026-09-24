ARG BUILD_FROM

# Build stage: pip is only needed to fetch dbus-fast and is discarded afterwards.
FROM $BUILD_FROM AS build
RUN apk add --no-cache python3 py3-pip \
    && pip3 install --no-cache-dir --no-compile --break-system-packages \
        --target /opt/pydeps dbus-fast

FROM $BUILD_FROM
RUN apk add --no-cache python3
COPY --from=build /opt/pydeps /opt/pydeps
ENV PYTHONPATH=/opt/pydeps

COPY relay.py /relay.py
COPY run.sh /run.sh
RUN chmod a+x /run.sh

CMD [ "/run.sh" ]
