#!/bin/sh

cargo build  --release --locked --target x86_64-unknown-linux-musl

# rsync binary to example-relay-free & restart
# rsync -az --info=progress2 ./target/x86_64-unknown-linux-musl/release/earendil root@45.33.109.28:/usr/local/bin/
# ssh root@45.33.109.28 'systemctl restart earendil'

# rsync binary to example-relay-paid & restart
rsync -az --info=progress2 ./target/x86_64-unknown-linux-musl/release/earendil root@172.233.162.12:/usr/local/bin/
ssh root@172.233.162.12 'systemctl restart earendil'