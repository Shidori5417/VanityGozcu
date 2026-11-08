# Yapılandırma

Ayarlar `config.toml` dosyasından, gizli bilgiler **yalnızca ortam değişkenlerinden** okunur. Config dosyasına token yazılırsa program uyarı verip başlamayı reddeder.

## config.example.toml

```toml
[general]
default_interval_secs = 30      # varsayılan kontrol aralığı
jitter_percent = 15             # aralığa ±% rastgelelik
confirm_checks = 2              # "boşaldı" demek için ardışık 404 sayısı
request_timeout_ms = 5000
database_path = "data/vanity.db"
log_dir = "logs"
log_level = "info"              # trace | debug | info | warn | error

[ratelimit]
max_requests_per_minute = 40    # tüm hedefler için ortak bütçe
backoff_max_secs = 300          # ardışık hatalarda üst sınır

[notify]
on_free = true
on_taken = false                # boştaki URL tekrar alınınca da haber ver
on_claim = true
on_error = true                 # checker uzun süre çalışamazsa

[notify.discord]
enabled = true
# URL: VG_DISCORD_WEBHOOK
mention = "<@123456789012345678>"   # opsiyonel ping

[notify.telegram]
enabled = true
# Token: VG_TELEGRAM_TOKEN
chat_id = "123456789"

[auto_claim]
enabled = false                 # bilerek açılmalı, bkz. GUVENLIK.md
# Token: VG_USER_TOKEN
target_guild_id = "000000000000000000"
max_attempts = 3
confirm_checks_override = 1     # auto-claim açıkken onay sayısı

[[targets]]
code = "ornekkod"
interval_secs = 20              # opsiyonel, varsayılanı ezer
priority = "high"               # high | normal | low
auto_claim = true               # bu hedef için otomatik alma (global açıksa)

[[targets]]
code = "baskakod"
auto_claim = false              # sadece bildirim
```

## Ortam değişkenleri

| Değişken | Zorunlu mu | Açıklama |
|---|---|---|
| `VG_DISCORD_WEBHOOK` | Discord bildirimi açıksa | Webhook URL'si |
| `VG_TELEGRAM_TOKEN` | Telegram açıksa | BotFather'dan alınan token |
| `VG_USER_TOKEN` | Yalnızca auto-claim açıksa | Kullanıcı hesabı token'ı |
| `VG_CONFIG` | Hayır | Config yolu (varsayılan `./config.toml`) |

systemd için bunlar `EnvironmentFile=/etc/vanity-gozcu/env` (izin `600`) ile verilir.

## Doğrulama kuralları

- `code`: 2–32 karakter, `[a-z0-9-]`, küçük harfe çevrilir, tekrar eden kod reddedilir.
- Hedef sayısı > 25 ise uyarı (bu araç az sayıda URL için tasarlandı).
- `max_requests_per_minute`, hedef sayısı ve aralıklarla tutarsızsa (bütçe yetmiyorsa) başlangıçta uyarı ve hesaplanan gerçek aralık gösterilir.
- `auto_claim.enabled = true` ama `VG_USER_TOKEN` yoksa: auto-claim kapalı başlar, TUI'de uyarı.

## TUI'den değişiklik

TUI'de eklenen/silinen hedefler ve mod değişiklikleri `config.toml`'a geri yazılır (yorumları korumak için `toml_edit` kullanılır).

## systemd örneği

```ini
[Unit]
Description=VanityGözcü
After=network-online.target

[Service]
ExecStart=/opt/vanity-gozcu/vanity-gozcu daemon
WorkingDirectory=/opt/vanity-gozcu
EnvironmentFile=/etc/vanity-gozcu/env
Restart=on-failure
RestartSec=5
User=vanity
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/opt/vanity-gozcu/data /opt/vanity-gozcu/logs

[Install]
WantedBy=multi-user.target
```
