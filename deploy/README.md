# Automatic updates

Runs the published image on a Linux machine with Docker and keeps it current:
every merge to `main` builds `ghcr.io/hulkhongan300/shamash:main` on GitHub,
and this machine pulls it within about five minutes and restarts the bot.
Nothing on the machine has to be reachable from outside.

```
change ──► pull request ──► checks + image build on GitHub
                                   │
                         review, press Merge
                                   │
                   ghcr.io/…/shamash:main is published
                                   │
         shamash-update.timer pulls it, restarts the bot (≤ 5 min)
```

## Install

From a checkout of this repo, on the machine that runs the bot:

```sh
sudo install -d -m 700 /opt/shamash
sudo install -m 644 deploy/compose.yaml /opt/shamash/compose.yaml
sudo install -m 600 .env /opt/shamash/.env          # your settings
sudo install -m 755 deploy/shamash-update.sh /usr/local/bin/shamash-update
sudo install -m 644 deploy/shamash-update.service deploy/shamash-update.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now shamash-update.timer
sudo systemctl start shamash-update.service         # first pull and start, now
```

Without a GPU, delete the two `devices` lines from `/opt/shamash/compose.yaml`.

## Day to day

| To | Run |
| --- | --- |
| See what the updater did | `journalctl -u shamash-update -n 20` |
| Watch the bot | `sudo docker compose -f /opt/shamash/compose.yaml logs -f` |
| Update right now | `sudo systemctl start shamash-update` |
| Pause updates | `sudo systemctl stop shamash-update.timer` |
| Stay on one version | set `SHAMASH_TAG=<tag>` in `/opt/shamash/.env` |
| Change settings | edit `/opt/shamash/.env`, then `sudo systemctl start shamash-update` |

The models, song cache and data live in the `shamash-data` volume, so an update
never downloads the models again. The previous image is removed once the new
one is running.
