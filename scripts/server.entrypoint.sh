#!/bin/sh

CONFIG_DIR=/data # Config directory

mkdir -p $CONFIG_DIR
export TOKI_DATA_DIR=$CONFIG_DIR

# Run the server
toki-server