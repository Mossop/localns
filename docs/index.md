# LocalNS

<p align="center" width="100%">
  <img src="logo.png">
</p>

LocalNS is a DNS server that serves records for names discovered from various
sources such as [Docker](https://www.docker.com/) and [Traefik](https://traefik.io/traefik/).
It is designed to need minimal configuration and will dynamically update as
names known to the sources change. When a record is not known it will fall
back to an upstream DNS server. It is aimed at running on a small local network
not as a public name server.

Put in a more practical way. If you run a bunch of containers in docker on a
network that is accessible then this will act as a nameserver, automatically
mapping names you define to whatever IP docker happens to pick when the
container starts. And docker is just one such source of names supported.

Unlike many other DNS servers LocalNS does not work on a zone by zone basis and
does not (unless configured to do so) claim to be the authority for any domain.
Instead the configured sources provide known names and if LocalNS is asked to
resolve a name it knows of it will respond. For anything else the query is
forwarded to upstream servers. This allows for internal services to use the same
domain name as other public services and allows LocalNS to respond with an
internal address for a name that also has a public address, a split horizon
configuration.

LocalNS is probably not production ready however it is running without issue as
the main name server for my own local network so is reasonably stable.

## Installation

There are two main ways to install LocalNS, ignoring downloading the source and
building it by hand.

Installing from `cargo` is fairly straightforward:

```shell
~$ cargo install localns
```

You can then execute `localns` but it's up to you to figure out doing this at
system startup and in the background etc. The [configuration file](configuration.md)
is selected either by passing a filename when running LocalNS, as the
`LOCALNS_CONFIG` environment variable or it will default to looking for
`config.yaml` in the working directory when launched.

The alternative is running in docker and letting docker handle system startup
which it may be configured to do already anyway:

```shell
~$ docker run -p 53:53/udp -d ghcr.io/mossop/localns
```

By default it will just forward all upstream requests to cloudflare's DNS over
TLS service. You can provide your own [configuration](configuration.md) by bind
mounting a file at `/etc/localns/config.yaml`.
