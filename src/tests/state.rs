use super::*;

#[test]
fn locking_is_per_guild_and_reports_changes() {
    let lock = ListenLock::default();
    let a = GuildId::new(1);
    let b = GuildId::new(2);
    assert!(!lock.is_locked(a));
    assert!(lock.set(a, true));
    assert!(!lock.set(a, true), "already locked");
    assert!(lock.is_locked(a));
    assert!(!lock.is_locked(b));
    assert!(lock.set(a, false));
    assert!(!lock.is_locked(a));
}
