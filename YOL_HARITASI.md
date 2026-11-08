# Yol haritası

Her aşama çalışan, test edilmiş bir sürümle biter.

## Aşama 1: Çekirdek izleme
- [x] Cargo projesi, `clap` komutları (`run`, `daemon`, `check`, `test-notify`)
- [x] `config.rs`: TOML + env yükleme ve doğrulama
- [x] `checker.rs`: invite endpoint, yanıt sınıflandırma, birim testleri (mock sunucu: `wiremock`)
- [x] `ratelimit.rs`: global bütçe + `Retry-After`
- [x] `scheduler.rs`: interval + jitter
- [x] `vanity-gozcu check <kod>` tek seferlik kontrol çalışır

## Aşama 2: Durum ve bildirim
- [x] `state.rs` durum makinesi, `confirm_checks` mantığı
- [x] `store.rs` SQLite: son durum + olay geçmişi
- [x] Discord webhook (embed: kod, eski sahip, zaman)
- [x] Telegram Bot API
- [x] Yeniden başlatmada tekrar bildirim gönderilmiyor (test)
- [x] `daemon` modu + systemd dosyası

## Aşama 3: TUI
- [x] Ana tablo + olay günlüğü
- [x] URL ekle/sil, şimdi kontrol et, test bildirimi
- [x] Değişiklikleri `toml_edit` ile config'e geri yazma
- [x] Terminal yeniden boyutlandırma, temiz çıkış (panic'te terminali geri yükleme)

## Aşama 4: Otomatik alma
- [x] `claimer.rs`: PATCH isteği, yanıt sınıflandırma
- [x] Başlangıç ön kontrolü (sunucu özelliği, yetki)
- [x] Doğrulama istenince durma + acil bildirim
- [x] TUI'de mod değiştirme + risk onay penceresi
- [x] Hedef bazlı `auto_claim` açma/kapama

## Aşama 5: Cilalama
- [x] `daemon` + TUI'nin Unix socket ile bağlanması (`attach`; VDS'te servis çalışırken TUI ile izleme)
- [~] Prometheus benzeri basit metrikler: **bilerek atlandı** (opsiyoneldi; basitlik için eklenmedi)
- [x] `cargo clippy -- -D warnings`, release profili (`lto = true`, `codegen-units = 1`, `strip = true`)
- [~] `cargo audit`: CI'da çalışıyor (`.github/workflows/ci.yml`); yerel makinede `cargo-audit` kurulu olmadığı için henüz yerelde çalıştırılmadı
- [x] Kurulum betiği (`deploy/install.sh`) ve README güncellemesi
- [ ] Gerçek Discord'a karşı uçtan uca deneme: yapılmadı (geliştirme ortamından `discord.com` erişilemiyor); VDS'te elle doğrulanmalı
