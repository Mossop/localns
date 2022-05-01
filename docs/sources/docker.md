# docker

This source provides names for running containers. It relies on specific named
labels on the containers and sometimes the networks to assign names. It listens
for events on the docker host and as containers start and stop names are
discovered or dropped.

A container is assigned a name if it has a `localns.hostname` label. The value
of the label should be the full DNS name for the container. The IP address to
assign to the name is discovered in a few ways:

If the container also has a `localns.network` label then it should name a
network the container is attached to and then the IP address on that network is
used.

Otherwise LocalNS looks for what it considers to be "visible" docker networks.
These are networks using the `host`, `macvlan` or `ipvlan` drivers. You can also
set a label `localns.exposed=true` on a network to force LocalNS to consider it
to be visible. Assuming a container is attached to just one visible network then
that determines the IP address used.

If no valid network is found or if multiple valid networks are found then an
error will be logged and the container ignored.

## Configuration

You must configure how to connect to the docker host which may be local or
remote.

```yaml
sources:
  docker:
    local: {}
    http: http://mydocker.local
    pipe: /var/run/docker.sock
    tls:
      address: 10.10.2.3
      private_key: key.pem
      certificate: cert.pem
      ca: ca.pem
```

This configures four different docker sources.

The `local` source connects to the local docker host by OS specific means. On
Unix that means the pipe at `/var/run/docker.sock`.

The `http` source connects over insecure http.

The `pipe` source connects to a pipe at a specific location.

The `tls` source connects over secure TLS using the address and certificates
provided.
