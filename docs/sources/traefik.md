# traefik

Traefik is an edge router that accepts connections and then based on routing
configuration proxies those requires to other services like internal services,
docker containers etc. This source discovers the services that Traefik is
proxying and generates DNS records.

This source periodically queries the Traefik instance for its current routing
rules. It recognises rules of the form ``Host(`myhost.com`)`` or
``Host(`host1.com`, `host2.com`)``. Queries for the recognised hosts will
be answered with the IP or name of the Traefik server.

## Configuration

Configuration is straightforward:

```yaml
sources:
  traefik:
    url: http://10.3.4.5
```

Any discovered hosts will be served as A records to the given IP. A CNAME record
would be used if a named host was given in the URL.

It is also possible to override the record:

```yaml
sources:
  traefik:
    url: http://10.3.4.5
    address: 10.10.10.10
```
