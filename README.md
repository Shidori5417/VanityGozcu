# VanityGözcü

Discord vanity URL'lerini (`discord.gg/xxx`) izleyen, boşaldığında Discord webhook ve Telegram üzerinden haber veren, istenirse URL'yi kendi sunucuna otomatik almaya çalışan bir Rust aracı. VDS üzerinde 7/24 çalışmak üzere tasarlandı; kontrol ve izleme `ratatui` tabanlı bir TUI ile yapılır.

![VanityGözcü TUI](docs/tui.png)

*TUI görünümü (yerel sahte API ile örnek veriler).*

## Özellikler

- **İzleme:** En fazla ~20 vanity kodunu düzenli aralıklarla kontrol eder.
- **Bildirim:** URL boşalınca Discord webhook ve Telegram bot mesajı gönderir.
- **Otomatik alma (opsiyonel, varsayılan KAPALI):** Kullanıcı token'ı ile URL'yi hedef sunucuya atamayı dener.
- **TUI:** Canlı durum tablosu, olay günlüğü, URL ekleme/çıkarma, mod değiştirme.
- **Headless mod:** systemd servisi olarak TUI'siz çalışma; istediğin zaman `attach` ile TUI'ye bağlanma.
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
./target/release/vanity-gozcu run     # tek süreç: izleme + TUI
```

### Komutlar

| Komut | Ne yapar |
|---|---|
| `run` | İzlemeyi başlatır ve TUI'yi aynı süreçte açar. Çıkınca izleme de durur. |
| `daemon` | Başsız (headless) çalışır, günlük dosyasına log yazar ve `socket_path` üzerindeki Unix socket'te `attach` bağlantılarını bekler. |
| `attach` | Çalışan bir `daemon`'a socket ile bağlanıp TUI açar; `run` ile aynı tuşlar. TUI'den çıkmak daemon'u durdurmaz. |
| `check <kod>` | Tek bir kodu bir kez kontrol eder. |
| `test-notify` | Açık bildirim kanallarına test mesajı gönderir. |

`daemon` çalışırken `run` başlatılmaz (iki motor aynı config'e ve veritabanına yazmasın diye); `run` bunu algılayıp `attach` önerir. Socket dosyası `0600` izinlidir, yalnızca aynı kullanıcı bağlanabilir; `daemon` kapanınca silinir. Bağlantı protokolü satır başına bir JSON mesajıdır (istekler `{"id":1,"cmd":"get_snapshot"}` biçimindedir).

### tmux olmadan kullanım

Arka planda süren bir `daemon` ile ekran yöneticisine (tmux/screen) gerek kalmaz:

```bash
./target/release/vanity-gozcu daemon &   # ya da systemd servisi olarak
./target/release/vanity-gozcu attach     # istediğin zaman bağlan, `q` ile ayrıl
```

`attach` ile yaptığın hedef ekleme/silme ve mod değişiklikleri daemon tarafından config'e yazılır.

## VDS'e kurulum

Discord Türkiye'den engelli olduğu için aracın 7/24 çalışacağı makinenin Discord'a erişebildiği bir ülkedeki bir VDS olması gerekir; bunun dışında bir yönlendirme yapılmaz. Linux VDS'te:

```bash
cargo build --release
sudo sh deploy/install.sh
```

Betik `vanity` sistem kullanıcısını, `/opt/vanity-gozcu/{data,logs}` dizinlerini, ikiliyi, örnek config'i (`/opt/vanity-gozcu/data/config.toml`, varsa dokunmaz), boş bir `/etc/vanity-gozcu/env` şablonunu (`600`) ve systemd birimini kurar. Servisi **başlatmaz**; sonraki adımları ekrana yazar: config'i ve `env` dosyasını doldur, `systemctl enable --now vanity-gozcu`, sonra izlemek için:

```bash
cd /opt/vanity-gozcu && sudo -u vanity env VG_CONFIG=/opt/vanity-gozcu/data/config.toml ./vanity-gozcu attach
```

## Belgeler

| Dosya | İçerik |
|---|---|
| [MIMARI.md](MIMARI.md) | Modüller, veri akışı, durum makinesi, crate seçimleri |
| [YAPILANDIRMA.md](YAPILANDIRMA.md) | `config.toml` alanları ve ortam değişkenleri |
| [GUVENLIK.md](GUVENLIK.md) | Token yönetimi, loglama, risk sınırları |
| [YOL_HARITASI.md](YOL_HARITASI.md) | Aşama aşama geliştirme planı |
| [PROMPT.md](PROMPT.md) | Projeyi bir AI kod asistanına yazdırmak için hazır prompt |
