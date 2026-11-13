# Putting the hub behind nginx

Written to match the vhost conventions already on this host (`nexnet`,
`homehunter`): bind the specific WAN address, keep the ACME challenge path,
include letsencrypt's shared TLS options, one log pair per vhost.

## Install

```bash
# `map` must live in the http block, not in a vhost
cp nexmesh-upgrade.conf /etc/nginx/conf.d/

cp nexmesh.example.net.conf /etc/nginx/sites-available/
ln -sf ../sites-available/nexmesh.example.net.conf /etc/nginx/sites-enabled/
nginx -t && service nginx reload
```

The certificate first, since the vhost references it:

```bash
certbot certonly --webroot -w /var/www/html -d nexmesh.example.net
```

nginx also needs its own client certificate for the hub:

```bash
nexmesh-hub --db /var/lib/nexmesh/hub.db pki issue nginx-proxy \
    --dir /var/lib/nexmesh/pki --out /etc/nexmesh/pki
```

## The part that is easy to get wrong

This vhost proxies to **https**, unlike the other vhosts here, because the hub
serves TLS on every address it listens on -- the machines connecting over the
LAN require it, since a node refuses to send its enrolment token in cleartext.
So nginx presents a client certificate of its own.

That connection therefore presents *nginx's* identity, not the visitor's. The
hub is started with `--trusted-proxy 127.0.0.1` and believes the forwarded
`X-Client-Verify` / `X-Client-CN` instead; without that flag the headers are
ignored and the certificate-to-token binding is simply not enforced. With it,
**the hub must not be reachable except through nginx or over the LAN** -- a
client that could reach it directly could set those headers itself. That is
why it binds `127.0.0.1` and `10.0.0.10` and never `wan0`.

Pairing is not proxied, on purpose: a machine being paired has no certificate
yet, so that endpoint cannot require one. It lives on the LAN address only.
