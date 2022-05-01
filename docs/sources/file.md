# file

The simplest of sources just provides names from a local YAML file. Each entry
in the file is a fully-qualified domain name mapped to either an IP address or
a name for a CNAME record:

```yaml
foo.mossop.dev: 10.10.4.5
bar.mossop.dev: foo.mossop.dev
```

## Configuration

Simply provide the path to the zone file:

```yaml
sources:
  file:
    mossop: mossop.yaml
```
