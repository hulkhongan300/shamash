use super::*;
const BOT_CHANNEL: u64 = 4242;
const OTHER_CHANNEL: u64 = 9;

fn may_drive(channel: Option<u64>) -> bool {
    VoiceService::member_is_in(channel.map(ChannelId::new), ChannelId::new(BOT_CHANNEL))
}

#[test]
fn only_members_in_the_bot_channel_may_drive_playback() {
    assert!(may_drive(Some(BOT_CHANNEL)), "in the channel, so allowed");
    assert!(!may_drive(Some(OTHER_CHANNEL)), "in another channel");
    assert!(!may_drive(None));
}

#[test]
fn an_occupied_channel_is_joined_without_anyone_moving() {
    assert_eq!(
        VoiceService::what_to_do(1, None),
        Presence::Join,
        "one person already there and the bot outside"
    );
    assert_eq!(
        VoiceService::what_to_do(1, Some(1)),
        Presence::None,
        "already where it should be"
    );
}

/// Rejoining a live channel on every sync would tear down the call.
#[test]
fn only_an_emptied_channel_is_left() {
    assert_eq!(VoiceService::what_to_do(0, Some(0)), Presence::Leave);
    assert_eq!(
        VoiceService::what_to_do(0, None),
        Presence::None,
        "nothing to leave"
    );
    assert_eq!(VoiceService::what_to_do(2, Some(2)), Presence::None);
}

#[test]
fn a_bot_moved_elsewhere_leaves_when_that_channel_empties() {
    assert_eq!(VoiceService::what_to_do(3, Some(1)), Presence::None);
    assert_eq!(VoiceService::what_to_do(3, Some(0)), Presence::Leave);
    assert_eq!(VoiceService::what_to_do(0, Some(2)), Presence::None);
}
