ARG BUILD_FROM
FROM $BUILD_FROM

RUN apk add --no-cache python3 py3-pip py3-virtualenv \
    && python3 -m venv /opt/venv \
    && /opt/venv/bin/pip install --no-cache-dir dbus-fast

COPY relay.py /relay.py
COPY run.sh /run.sh
RUN chmod a+x /run.sh

CMD [ "/run.sh" ]
