#!/usr/bin/with-contenv bashio

export REMOTE_MAC=$(bashio::config 'remote_mac')
export NAD_HOST=$(bashio::config 'nad_host')
export NAD_PORT=$(bashio::config 'nad_port')
export TV_HOST=$(bashio::config 'tv_host')
export TV_PSK=$(bashio::config 'tv_psk')

bashio::log.info "Starting NAD/Bravia remote bridge (remote ${REMOTE_MAC})"

exec /opt/venv/bin/python3 /relay.py
