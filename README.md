# Qubes Notification Proxy

Qubes Notification Proxy is part of [Qubes OS](https://www.qubes-os.org). It lets
a program running in a qube show desktop notifications on the host, without
giving the qube direct access to the host's notification daemon. The proxy
forwards messages over qrexec, so a notification from an untrusted qube can never
reach the real `org.freedesktop.Notifications` service itself.

## How it works

The proxy has two parts:

- **Agent** (guest side). A systemd user service runs
  `qubes-notification-proxy-client`, which claims the `org.freedesktop.Notifications`
  name on the qube's session bus. Programs in the qube send notifications to it as
  usual; it relays each one over the `qubes.Notifications` qrexec service.
- **Daemon** (host side). The `qubes.Notifications` service runs
  `qubes-notification-proxy-server`, which re-emits the notification to the host's
  real notification daemon. It prefixes the text with the qube's name and adds the
  qube's colored mark, so you can see which qube a notification came from.

The agent service is enabled automatically and only starts for users in the
`qubes` group.
