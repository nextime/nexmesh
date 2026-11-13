# Putting the hub behind nginx

`map` has to live in the `http` block, not in a vhost, so add this once —
Debian/Devuan: `/etc/nginx/conf.d/nexmesh-upgrade.conf`:

```nginx
map $http_upgrade $connection_upgrade {
    default upgrade;
    ''      close;
}
```

Then install the vhost and reload:

```bash
cp nexmesh.nexlab.net.conf /etc/nginx/sites-available/
ln -sf ../sites-available/nexmesh.nexlab.net.conf /etc/nginx/sites-enabled/
nginx -t && systemctl reload nginx   # or: service nginx reload
```

## The part that is easy to get wrong

The vhost forwards `X-Client-Verify` and `X-Client-CN`, and the hub only
believes them when started with `--trusted-proxy 127.0.0.1` (or whichever
address nginx connects from). Without that flag the hub ignores the headers
entirely and falls back to its own handshake — which, behind a proxy, means no
certificate at all, so the binding is simply not enforced.

That flag is also why **the hub must not be reachable except through nginx**.
A client that can reach it directly can set `X-Client-CN` itself and claim any
identity. Bind the hub to `127.0.0.1` and let nginx be the only way in.

The pairing listener is a separate address on purpose: bind it to the LAN or
the VPN (`--pair-listen 192.168.42.44:7871`), never to the public interface.
