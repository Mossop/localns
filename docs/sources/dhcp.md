# dhcp

This source provides names from a dnsmasq lease file. Dnsmasq already does this
of course but this allows you to bypass that functionality.

## Configuration

Configuration requires giving the path to the lease file (currently must be an
absolute path) and the zone name:

```yaml
sources:
  dhcp:
    leases:
      lease_file: /var/lib/dnsmasq.leases
      zone: local.mossop.dev
```

LocalNS will watch the lease file for changes reload the data very quickly.
