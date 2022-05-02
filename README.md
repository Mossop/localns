# LocalNS

<p align="center" width="100%">
  <img src="logo/logo256.png">
</p>

[![Documentation Status](https://readthedocs.org/projects/localns/badge/?version=latest)](https://localns.readthedocs.io/en/latest/?badge=latest)
[![Tests](https://github.com/Mossop/localns/actions/workflows/test.yml/badge.svg)](https://github.com/Mossop/localns/actions/workflows/test.yml)

LocalNS is a DNS server that serves records for names discovered from various
sources such as [Docker](https://www.docker.com/) and [Traefik](https://traefik.io/traefik/).
It is designed to need minimal configuration and will dynamically update as
names known to the sources change. When a record is not known it will fall
back to an upstream DNS server.

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

[Read the documentation](https://localns.readthedocs.io/en/latest) for more on
how to install and use LocalNS.
