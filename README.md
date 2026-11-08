# VanityGözcü

Discord vanity URL'lerini (`discord.gg/xxx`) izleyen, boşaldığında Discord webhook ve Telegram üzerinden haber veren, istenirse URL'yi kendi sunucuna otomatik almaya çalışan bir Rust aracı. VDS üzerinde 7/24 çalışmak üzere tasarlandı; kontrol ve izleme `ratatui` tabanlı bir TUI ile yapılır.

## Özellikler

- **İzleme:** En fazla ~20 vanity kodunu düzenli aralıklarla kontrol eder.
- **Bildirim:** URL boşalınca Discord webhook ve Telegram bot mesajı gönderir.
- **Otomatik alma (opsiyonel, varsayılan KAPALI):** Kullanıcı token'ı ile URL'yi hedef sunucuya atamayı dener.
- **TUI:** Canlı durum tablosu, olay günlüğü, URL ekleme/çıkarma, mod değiştirme.
- **Headless mod:** systemd servisi olarak TUI'siz çalışma.
- **Rate limit'e saygı:** Discord'un `Retry-After` ve `X-RateLimit-*` başlıklarına uyar.

## Önemli uyarı

Bildirim modu token gerektirmez ve Discord kurallarıyla sorun yaratmaz.

Otomatik alma modu **kullanıcı hesabı token'ı** gerektirir. Kullanıcı hesabını otomasyonla kullanmak (self-bot) Discord Hizmet Şartları'na aykırıdır ve hesabın kapatılmasına yol açabilir. Bu mod bilerek açılmalı; mümkünse ana hesap yerine bu riski kabul ettiğin bir hesap kullan. Araç MFA / captcha gibi doğrulamaları **atlatmaya çalışmaz**; böyle bir adım istenirse durur ve bildirim atar.

## Hızlı başlangıç

```bash
cargo build --release
cp config.example.toml config.toml   # düzenle
export VG_TELEGRAM_TOKEN=...          # gizli bilgiler env'den okunur
export VG_DISCORD_WEBHOOK=...
./target/release/vanity-gozcu run     # TUI ile
./target/release/vanity-gozcu daemon  # headless (systemd)
```

## Belgeler

| Dosya | İçerik |
|---|---|
| [MIMARI.md](MIMARI.md) | Modüller, veri akışı, durum makinesi, crate seçimleri |
| [YAPILANDIRMA.md](YAPILANDIRMA.md) | `config.toml` alanları ve ortam değişkenleri |
| [GUVENLIK.md](GUVENLIK.md) | Token yönetimi, loglama, risk sınırları |
| [YOL_HARITASI.md](YOL_HARITASI.md) | Aşama aşama geliştirme planı |
| [PROMPT.md](PROMPT.md) | Projeyi bir AI kod asistanına yazdırmak için hazır prompt |
