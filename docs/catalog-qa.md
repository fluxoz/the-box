# Catalog QA results

Every template below was deployed on a real Box, checked the way a person
would check it (the app's own page through the Box's front door, with its
title; or the engine answering its own protocol), and then deleted. Run on
box-ccfa06 against shipped releases, 100 templates.

- verified working: **96**
- GPU-only (this box has no GPU, marked `gpu = true`): **4** (comfyui, llamacpp, tabby, vllm)

## What this QA found and fixed

Platform bugs, each found by using a template rather than reading it:

- one crash-looping container failed every later deploy and blocked updates
- generated credentials ignored the key length an app requires (Firefly III)
- two services from one template shared a data directory
- a redeploy minted a new password against existing data, locking you out
- bind mounts were unwritable to every non-root image
- proxied apps got no websocket upgrade or forwarded headers
- a preset that fails to parse is skipped silently (lint now catches it)

## Verified templates

- actualbudget
- adminer
- anythingllm
- audiobookshelf
- babybuddy
- chroma
- clickhouse
- code-server
- couchdb
- cyberchef
- dashdot
- dokuwiki
- drawio
- esphome
- excalidraw
- firefly-iii
- focalboard
- forgejo
- freshrss
- gitea
- gonic
- gotify
- grafana
- grocy
- healthchecks
- home-assistant
- homebox
- homebridge
- homepage
- httpbin
- infinity
- influxdb
- it-tools
- jellyfin
- jenkins
- jupyter
- kavita
- kokoro
- komga
- langflow
- letta
- librespeed
- libretranslate
- linkding
- litellm
- localai
- mailpit
- mariadb
- mealie
- meilisearch
- memos
- metube
- minio
- mongodb
- mosquitto
- music-assistant
- mysql
- n8n
- nats
- navidrome
- neo4j
- nextcloud
- node-red
- ntfy
- ollama
- open-webui
- pgvector
- photoprism
- phpmyadmin
- piper-tts
- postgres
- prometheus
- qdrant
- questdb
- rabbitmq
- redis
- registry
- rethinkdb
- searxng
- shiori
- sillytavern
- sonarqube
- speedtest-tracker
- stirling-pdf
- syncthing
- tandoor
- timescaledb
- typesense
- uptime-kuma
- valkey
- vaultwarden
- verdaccio
- vikunja
- wallabag
- whisper-asr
- wiremock
