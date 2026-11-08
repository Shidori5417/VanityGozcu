# Yol haritası

Her aşama çalışan, test edilmiş bir sürümle biter.

## Aşama 1: Çekirdek izleme
- [ ] Cargo projesi, `clap` komutları (`run`, `daemon`, `check`, `test-notify`)
- [ ] `config.rs`: TOML + env yükleme ve doğrulama
- [ ] `checker.rs`: invite endpoint, yanıt sınıflandırma, birim testleri (mock sunucu: `wiremock`)
- [ ] `ratelimit.rs`: global bütçe + `Retry-After`
- [ ] `scheduler.rs`: interval + jitter
- [ ] `vanity-gozcu check <kod>` tek seferlik kontrol çalışır

## Aşama 2: Durum ve bildirim
- [ ] `state.rs` durum makinesi, `confirm_checks` mantığı
- [ ] `store.rs` SQLite: son durum + olay geçmişi
- [ ] Discord webhook (embed: kod, eski sahip, zaman)
- [ ] Telegram Bot API
- [ ] Yeniden başlatmada tekrar bildirim gönderilmiyor (test)
- [ ] `daemon` modu + systemd dosyası

## Aşama 3: TUI
- [ ] Ana tablo + olay günlüğü
- [ ] URL ekle/sil, şimdi kontrol et, test bildirimi
- [ ] Değişiklikleri `toml_edit` ile config'e geri yazma
- [ ] Terminal yeniden boyutlandırma, temiz çıkış (panic'te terminali geri yükleme)

## Aşama 4: Otomatik alma
- [x] `claimer.rs`: PATCH isteği, yanıt sınıflandırma
- [x] Başlangıç ön kontrolü (sunucu özelliği, yetki)
- [x] Doğrulama istenince durma + acil bildirim
- [x] TUI'de mod değiştirme + risk onay penceresi
- [x] Hedef bazlı `auto_claim` açma/kapama

## Aşama 5: Cilalama
- [ ] `daemon` + TUI'nin Unix socket ile bağlanması (VDS'te servis çalışırken TUI ile izleme)
- [ ] Prometheus benzeri basit metrikler (opsiyonel)
- [ ] `cargo clippy -- -D warnings`, `cargo audit`, release profili (`lto = true`, `codegen-units = 1`, `strip = true`)
- [ ] Kurulum betiği ve README güncellemesi
