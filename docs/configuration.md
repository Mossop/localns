# Configuration

LocalNS is configured with a single YAML file. Sorry. This project started life
as purely docker focused so YAML made sense back then.

When executing the `localns` binary directly the first argument will tell it
where to look for the file, otherwise it will use the value of the
`LOCALNS_CONFIG` environment variable or look for a `config.yaml` file in the
current directory.

If you're using the provided docker container than the config file is located at
`/etc/localns/config.yaml`.

In some places in the file paths to other files can be given. In these places
relative paths are taken as relative to the config file.

The configuration file is automatically reloaded moments after making any
changes, no need to restart the server.

## DNS Server

By default LocalNS will listen for requests over both TCP and UDP protocols on
port 53. You can override this in the configuration:

```yaml
server:
  port: 5353
```

## Zones

Zones or domains are the building blocks of DNS. Any name lookup is part of one.
The answer for `www.google.com` would generally be found in the `google.com.`
zone for instance (though could also be in the `com.` zone).

In LocalNS zones help configure how to respond when an answer exists for a name
and what to do when no answer exists. There are two important configuration
sections:

```yaml
defaults:
  upstream: 1.1.1.1

zones:
  somewhere.com:
    ttl: 400
  mossop.dev:
    upstream: 2.2.2.2
    ttl: 300
    authoratative: true
  somewhere.mossop.dev:
    ttl: 3600
```

Zones inherit configuration based on the domain name hierarchy, so in this
example `somewhere.mossop.dev` inherits the configiuration from `mossop.dev`
replacing the ttl with a new value. The `defaults` can be thought of as the
root zone, all other zones inherit from it so the upstream for `somewhere.com`
is `1.1.1.1`.

The actual confgurations available for each zone (or defaults) are:

* **upstream** configures an upstream DNS server for when a query for an unknown
  name is received. This can be a `host:port` pair but currently must be a UDP
  DNS server.
* **ttl** sets the default ttl for answers which may be overridden by the source
  that provided the answer.
* **authoratative** configures whether LocalNS is authoratative for the zone.
  This affects some details in the answer and unless LocalNS is being used as
  the upstream for another DNS server is probably unimportant.

### Upstream DNS Servers

Currently LocalNS only supports the most basic of upstream servers, a single UDP
server. It would be wise to use something like a local CoreDNS instance as a the
upstream which can then be configured with multiple upstreams. In fact the
docker container includes a running CoreDNS instance at `127.0.0.1:58` for this
very reason. By default it forwards to Cloudflare's TLS server but the
configuration file at `/etc/coredns/Corefile` can be changed to whatever you
like.

## Sources

Configuring the sources involves adding a section for the source type, a short
name for the source and then the configuration. This allows multiple instances
of the same type of source with different configurations.

This configures two file sources, one named **main** and another named
**second**:

```yaml
sources:
  file:
    main: main.yaml
    second: second.yaml
```

The names are unimportant beyond their use in log messages.

Several different sources are available:

* **[file](sources/file.md)**: Loads names from a simple YAML file.
* **[docker](sources/docker.md)**: Loads names from running docker containers.
* **[traefik](sources/traefik.md)**: Loads names from the [Traefik](https://traefik.io/traefik/) reverse proxy.
* **[dhcp](sources/dhcp.md)**: Loads names from a DHCP lease file.
* **[remote](sources/remote.md)**: Loads names from a remote LocalNS instance.

## Loopback DNS

It is possible that one source needs to resolve a name provided by another
source. A good example is when the docker source provides the name for the
traefik source. LocalNS does not currently handle this automatically but it
works as long as you set the DNS server for the computer LocalNS is running on
to be LocalNS itself. In the standalone case that would mean running LocalNS on
port 53 and setting the computer's name server to `127.0.0.1`. In the docker case
passing `--dns 127.0.0.1` to the docker run command suffices.

The only downside to such a configuration is that you may occasionally see
errors during startup or after a configuration change as the names required are
discovered. LocalNS attempts to discover the names from sources in a logical
order to avoid the chance of this but it isn't foolproof.
