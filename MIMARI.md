# Mimari

## Genel akış

```
                ┌──────────────┐
 config.toml ──►│   Scheduler  │── her URL için zamanlanmış kontrol
                └──────┬───────┘
                       ▼
                ┌──────────────┐   GET /api/v10/invites/{code}
                │   Checker    │──────────────────────────────► Discord API
                └──────┬───────┘
                       │ CheckResult
                       ▼
                ┌──────────────┐
                │ State Engine │── durum geçişi (Taken → Free vb.)
                └──┬───────┬───┘
         Event     │       │  Event (Free + auto_claim açık)
                   ▼       ▼
          ┌──────────┐  ┌──────────┐  PATCH /guilds/{id}/vanity-url
          │ Notifier │  │ Claimer  │──────────────────────────► Discord API
          └──────────┘  └──────────┘
          webhook + telegram
                   │
                   ▼
            ┌────────────┐
            │  TUI / Log │  (tokio::broadcast ile olayları dinler)
            └────────────┘
```

Tüm bileşenler tek binary içinde, `tokio` runtime üzerinde ayrı task'lar olarak çalışır ve `tokio::sync::{mpsc, broadcast, watch}` kanallarıyla haberleşir.

## Modüller

```
src/
├── main.rs            # clap CLI: run | daemon | check <code> | test-notify
├── config.rs          # TOML + env yükleme, doğrulama
├── model.rs           # VanityTarget, VanityState, Event, CheckResult
├── scheduler.rs       # interval + jitter, URL başına zamanlama
├── checker.rs         # invite endpoint isteği, yanıt sınıflandırma
├── ratelimit.rs       # global + bucket bazlı limit takibi
├── state.rs           # durum makinesi, geçiş tespiti, kalıcı durum
├── claimer.rs         # opsiyonel otomatik alma
├── notify/
│   ├── mod.rs         # Notifier trait, fan-out
│   ├── discord.rs     # webhook (embed)
│   └── telegram.rs    # Bot API sendMessage
├── store.rs           # SQLite: geçmiş olaylar, son durumlar
└── tui/
    ├── mod.rs         # uygulama döngüsü, input
    ├── table.rs       # URL durum tablosu
    ├── log.rs         # olay günlüğü paneli
    └── dialogs.rs     # URL ekle/sil, mod değiştir, onay pencereleri
```

## Kontrol mantığı (checker)

`GET https://discord.com/api/v10/invites/{code}` (kimlik doğrulamasız):

| Yanıt | Anlam | Sonuç |
|---|---|---|
| 200 | Kod bir sunucuya ait | `Taken { guild_id, guild_name }` |
| 404 + JSON `code: 10006` | Bilinmeyen davet | `Free` |
| 429 | Rate limit | `RateLimited { retry_after }` → bekle, sonucu sayma |
| 5xx / timeout / ağ hatası | Geçici hata | `Error` → geri çekilme (backoff) |
| Diğer | Beklenmeyen | `Unknown`, logla |

**Yanlış pozitif koruması:** Tek bir 404 hemen "boşaldı" sayılmaz. `confirm_checks` (varsayılan 2) ardışık `Free` sonucu gelince geçiş onaylanır. Otomatik alma açıksa bu değer 1'e düşürülebilir (ayarlanabilir).

## Durum makinesi

```
Unknown ──► Taken ──► Free(şüpheli) ──► Free(onaylı) ──► Claimed / Lost
               ▲            │                   │
               └────────────┘ (tekrar 200)      └─► Taken (başkası aldı)
```

Olaylar: `BecameFree`, `BecameTaken`, `ClaimSucceeded`, `ClaimFailed { reason }`, `ClaimNeedsVerification`, `CheckerDegraded`.

Son durum SQLite'a yazılır; yeniden başlatmada aynı bildirim tekrar gönderilmez.

## Zamanlama

- Her hedef için `interval_secs` + rastgele jitter (±%15).
- Hedeflere öncelik verilebilir (`priority = high` → daha kısa aralık).
- Global istek bütçesi: `ratelimit.rs` tüm kontrolleri tek bir token bucket'tan geçirir; 429 gelirse **tüm** kontroller `Retry-After` kadar durur.
- Proxy rotasyonu veya rate limit atlatma **yoktur**; 20 URL için gerekmez.

## Otomatik alma (claimer)

Sadece `auto_claim.enabled = true` ve token tanımlıysa çalışır.

1. `BecameFree` olayı gelir.
2. `PATCH https://discord.com/api/v10/guilds/{target_guild_id}/vanity-url` gövde `{"code": "<kod>"}`, `Authorization: <kullanıcı token>`.
3. Sonuç sınıflandırılır:
   - 200 → `ClaimSucceeded`, URL `Claimed` durumuna geçer, izleme listesinde "sahibi biziz" olarak işaretlenir.
   - 400/403 (yetki yok, sunucuda VANITY_URL özelliği yok, kod alınmış) → `ClaimFailed`.
   - MFA / captcha / ek doğrulama isteği → `ClaimNeedsVerification`, **deneme durdurulur**, acil bildirim gönderilir.
   - 429 → `Retry-After` kadar bekle, en fazla `max_attempts` dene.
4. Başlangıçta ön kontrol: hedef sunucu bilgisi çekilip `VANITY_URL` özelliği ve token'ın erişimi doğrulanır; sorun varsa TUI'de kırmızı uyarı gösterilir.

Gereksinim: hedef sunucuda vanity özelliği (Boost seviye 3 veya Discord'un verdiği özellik) ve hesapta `MANAGE_GUILD` yetkisi.

## TUI

- **Ana tablo:** kod, durum, son kontrol, sonraki kontrol, sahip sunucu adı, otomatik alma aktif mi.
- **Alt panel:** olay günlüğü (renkli: boşaldı = yeşil, hata = kırmızı).
- **Kısayollar:** `a` ekle, `d` sil, `c` şimdi kontrol et, `m` modu değiştir (bildirim / otomatik alma, onay penceresiyle), `t` test bildirimi, `q` çıkış.
- VDS'te `tmux` içinde çalıştırılır. İleri aşamada: `daemon` + TUI'nin Unix socket ile bağlanması (YOL_HARITASI aşama 5).

## Crate seçimleri

| Amaç | Crate |
|---|---|
| Async runtime | `tokio` (full) |
| HTTP | `reqwest` (`rustls-tls`, `json`), keep-alive açık tek client |
| Serileştirme | `serde`, `serde_json`, `toml` |
| CLI | `clap` (derive) |
| TUI | `ratatui`, `crossterm` |
| Log | `tracing`, `tracing-subscriber`, `tracing-appender` (dosyaya döner log) |
| Veritabanı | `rusqlite` (bundled) veya `sqlx` (sqlite) |
| Hata | `thiserror` (kütüphane), `anyhow` (main) |
| Gizli veri | `secrecy` (token'ı loglarda göstermemek için) |
| Zaman | `chrono` / `time` |
| Rastgelelik | `rand` (jitter) |

## Performans hedefleri

- Boşta RAM < 20 MB, CPU ~%0.
- Tek `reqwest::Client`, HTTP/2 ve bağlantı havuzu yeniden kullanımı.
- Otomatik alma yolunda gereksiz await/allocation yok: claim isteği önceden hazırlanmış header'larla gönderilir.
