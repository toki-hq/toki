#!/bin/sh

CONFIG_DIR=/usr/local/toki # Config directory

mkdir -p $CONFIG_DIR
export TOKI_CONFIG=$CONFIG_DIR/config.toml

# Check if the config.toml file exists
if [ ! -f $CONFIG_DIR/config.toml ]; then
    # If TOKI_SERVER_PASSWORD is set, create the config.toml file
    if [ -n "$TOKI_SERVER_PASSWORD" ]; then
        echo "TOKI_SERVER_PASSWORD is set, creating config.toml file"
        echo "password = \"$TOKI_SERVER_PASSWORD\"" > $CONFIG_DIR/config.toml
    fi
fi

# Run the server
toki-server