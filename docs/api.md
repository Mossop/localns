# API

LocalNS can be configured to expose a very minimal HTTP API. This is really only
for the [remote source](sources/remote.md) functionality right now but can also
be useful for diagnosing issues.

For the time being the API should be considered to be unstable.

The API is enabled by adding something like the following to the configuration
file:

```yaml
api:
  address: 0.0.0.0:80
```

This gives the address and port to listen on `0.0.0.0` will listen on all
addresses.

## records

A GET request that returns the current known DNS records:

```shell
~$ curl http://localhost/records
[{"name":"docker.mossop.dev.","ttl":null,"rdata":{"A":"10.10.1.1"}}]
```

Note that records discovered from [remote instances](sources/remote.md) will not
be returned.
