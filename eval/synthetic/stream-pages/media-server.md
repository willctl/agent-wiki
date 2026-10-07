---
title: Media server
type: project
summary: "Jellyfin on the home lab's mini PC (media-01): libraries on the NAS, Intel Quick Sync transcoding, nightly config backups."
tags: ["home-lab","jellyfin","media"]
created: 2026-08-02T11:00:00-05:00
updated: 2026-09-28T21:40:00-05:00
updated_by: curator
---

# Media server

Sam runs Jellyfin for the household on a mini PC in the home lab ([[home-lab]]). It streams the movie, TV, music and photo libraries kept on the NAS to the living-room TV, two phones, a tablet and a laptop, inside the house and over the VPN ([[vpn-setup]]) when travelling.

## Hardware

- Host: `media-01`, a Beelink EQ12 mini PC with an Intel N100 (4 cores, Quick Sync), 16 GB of DDR5 and a 500 GB NVMe system disk.
- It sits on the shelf next to the NAS, on the same UPS (APC Back-UPS 700), which gives about 25 minutes of runtime for both.
- Network: one 2.5 GbE port, wired to port 6 of the home lab switch, VLAN 20 (servers). Static address 10.0.4.31, DNS name `media-01.lan`.
- Power draw: about 7 W idle and 18 W while transcoding two 4K streams.
- BIOS: "restore on AC power loss" is on, so it comes back by itself after an outage; Wake-on-LAN is off.

## Operating system

- Debian 12 (bookworm), minimal install, no desktop. Unattended upgrades install security updates nightly at 03:30.
- Jellyfin runs in Docker (compose file at `/opt/media/compose.yaml`), image `jellyfin/jellyfin:10.10`, pinned to a minor version; it is updated by hand after reading the release notes.
- The container runs as user `media` (uid 1100), which owns `/opt/media` and has read-only access to the NAS shares.
- The `render` group is passed into the container so Jellyfin can use `/dev/dri/renderD128` for hardware transcoding.
- Log rotation: Docker's json-file driver with max-size 20 MB and 3 files.

## Storage and libraries

- The libraries live on the NAS (Synology DS923+, see [[home-lab]]) and are mounted over NFS v4.1 at `/mnt/media`, read-only, with `hard,noatime` options and a systemd automount so a NAS reboot does not hang the boot.
- Movies: `/mnt/media/movies`, about 610 titles, 3.9 TB, one folder per film named "Title (Year)".
- TV: `/mnt/media/tv`, about 95 series, 5.2 TB, Season folders named "Season 01".
- Music: `/mnt/media/music`, 41,000 tracks in FLAC and MP3, 820 GB, tagged with MusicBrainz Picard.
- Photos: `/mnt/media/photos`, the family photo archive from 2004 onward, 1.1 TB, shown as a Jellyfin "home videos and photos" library.
- Metadata and the Jellyfin database live on the NVMe at `/opt/media/config` (about 14 GB with images); the transcode cache is on the NVMe at `/opt/media/cache`, capped at 40 GB by a weekly cleanup task.
- Library scans run every 6 hours and on demand; real-time monitoring is off because NFS does not deliver change notifications.

## Users and access

- Accounts: Sam (administrator), Alex (adult), and two kids' accounts with parental controls limited to ratings up to PG and TV-PG, and no access to the photo library.
- Passwords are in the family 1Password vault, item "Jellyfin accounts"; the administrator account is never used on the TV.
- Remote access is only over the VPN; the server is not exposed to the internet and there is no port forward on the router.
- Quick Connect is enabled so the TV and phones can sign in with a code instead of typing passwords.
- Each account has its own "continue watching" and watched state; the kids' accounts cannot delete media or change settings.

## Clients

- Living-room TV: an LG webOS TV with the Jellyfin app from the LG store; plays most files directly, needs transcoding for DTS audio.
- Phones: the Jellyfin app on iOS (Sam) and Android (Alex), with downloads allowed for offline viewing on trips.
- Tablet: Findroid on the kids' Android tablet, with downloads limited to 10 GB.
- Laptop: Jellyfin Media Player on Windows, which plays everything directly.
- Music: Finamp on both phones for the music library, with offline downloads of a few playlists.

## Transcoding

- Hardware acceleration: Intel QSV, with hardware decoding enabled for H.264, HEVC (8- and 10-bit), VP9 and AV1, and hardware encoding to H.264 and HEVC.
- Tone mapping: enabled through VPP, so HDR10 films play with correct colours on the phones; Dolby Vision profile 5 files fall back to software tone mapping and are slow.
- Throttling and segment deletion are on, so a paused stream stops using the CPU and the cache stays small.
- Measured on 2026-08-20: three simultaneous 4K HEVC to 1080p H.264 transcodes at about 70% GPU, CPU under 30%.
- Subtitles: text subtitles (SRT) are delivered as separate tracks; image subtitles (PGS) are burned in, which forces a transcode.
- Audio: clients that cannot play TrueHD or DTS get AAC stereo; the TV gets AC3 5.1 through the soundbar.

## Plugins

- Open Subtitles, signed in with Sam's account, downloads English subtitles for new films automatically.
- Playback Reporting keeps a history of who watched what, used to clear out films nobody watches.
- Intro Skipper detects TV intros and shows a "Skip intro" button; detection runs nightly at 02:00 for new episodes.
- TMDb Box Sets groups films into collections.
- Fanart provides logos and backgrounds that the TV client shows on the home screen.

## Backups

- Nightly at 04:00, a systemd timer stops the container, archives `/opt/media/config` (without the cache and the metadata images) to `/mnt/backup/jellyfin/` on the NAS, and starts the container again; the archive is about 600 MB and 14 are kept.
- The NAS backs the `jellyfin` backup folder up offsite with Hyper Backup, with the rest of the backups (see [[restore-drill]]).
- The media files themselves are not backed up offsite, except photos and music: films and TV can be re-ripped, the photo archive cannot.
- Restore tested on 2026-09-06: a fresh container with the restored config came up with all users, watched state and settings in 9 minutes.

## Network and naming

- Inside the house: http://media-01.lan:8096, also reachable as http://jellyfin.lan through the reverse proxy on the NAS (Synology's built-in nginx), which adds HTTPS with the house certificate for `*.lan`.
- Over the VPN: the same names, because the VPN pushes the house DNS server.
- The server announces itself on the LAN with DLNA turned off and client discovery on, so the apps find it without typing an address.

## Maintenance

- Updates: Debian security updates are automatic; Jellyfin minor updates are applied by hand on Sunday mornings after a config backup, by changing the image tag in the compose file and running `docker compose pull && docker compose up -d`.
- Disk health: smartd on media-01 emails Sam when the NVMe reports errors; the NAS checks its own disks monthly.
- Library hygiene: once a quarter Sam removes films nobody has watched in two years, using the Playback Reporting history, after asking Alex.
- Monitoring: Uptime Kuma on the NAS checks http://media-01.lan:8096/health every minute and sends a Pushover alert after three failures.

## Known issues

- After a NAS reboot the NFS mount sometimes comes back stale; restarting the automount (`systemctl restart mnt-media.automount`) fixes it, and the container needs a restart after that.
- The LG app cannot play some files with PGS subtitles without stuttering, because burning in the subtitles at 4K maxes out the transcoder; the workaround is to pick SRT subtitles.
- Intro Skipper misses intros shorter than 15 seconds.

## Open items

- Try the Jellyfin 10.11 beta once Intro Skipper supports it.
- Decide whether to add a second NVMe for the transcode cache.

## History

- 2026-08-20: Moved from the old Raspberry Pi 4 (software transcoding only) to media-01.
- 2026-09-06: Config backups moved from a USB stick to the NAS.
