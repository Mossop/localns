# remote

This source discovers records from other LocalNS instances. This allows for
services separated by a VPN say to discover each other. If the VPN goes down
then services on the local side can still be seen.

Names discovered from remote instances are not passed along to other instances
so it is quite possible to configure a loop where two LocalNS instances
configure each other as remotes with no problem.

This source requires that the remote instance have the [API](../api.md) enabled.

## Configuration

You just need to provide the http (or https) url of the remote instance:

```yaml
sources:
  remote:
    myremote:
      url: http://10.10.3.4
```
