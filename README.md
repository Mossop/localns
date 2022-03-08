# localns

This is a DNS server running in a docker container that serves DNS records
generated from a number of sources optionally with upstream servers serving
anything unknown. It support split-horizon DNS so that local IPs can be served
for local servers when the public authoratative nameserver serves a different
IP.

I am building this to solve some complexity I am facing on my local network
where I want DNS served for various local docker containers and other services.

With no configuration the server will serve nothing. You must add sources of DNS
records and optionally an upstream for everything else. A number of sources will
be supported starting with docker containers, traefik instances and dnsmasq
lease files. Others should be straightforward to add.

## Configuration

Configuration is done via a yaml file located at `/etc/localns/config.yaml`:

```yaml
upstream: 1.1.1.1
sources:
  docker:
    local: {}
zones:
  myzone.local:
    upstream: 10.10.10.254
  myotherzone.local:
    authoratative: true
```

The root upstream is the default for all detected zones and will be used when
queried for zones not known locally. It is optional.

The sources key lists the different sources of records, see below.

The zones key allows for fine-tuning of zone configuration. Zones will still be
generated for domains not listed here when records are discovered, they will
just use the default values.

For each zone you can set an upstream (if not set the top-level upstream is
used) or you can mark the zone as authoratative in which case no querying
upstream for unknown records will occur.

## Sources

Each source type allows for listing a number of sources with a name (mainly
used for logging) and a configuration.

### docker

Serves records based on docker containers. Once connected to a docker server
it watches for containers to start and stop. Any containers with the label
`localns.hostname` have a DNS record generated.

### traefik

Connects to a [Traefik](https://doc.traefik.io/traefik/) instance and creates
host records for all of the known http services.

### dhcp

Attempts to read hosts from a dhcp lease file (currently dnsmasq format is
supported).
