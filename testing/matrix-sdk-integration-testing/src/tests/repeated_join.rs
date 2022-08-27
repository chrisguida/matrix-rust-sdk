use std::time::Duration;

use anyhow::Result;
use assign::assign;
use matrix_sdk::{
    event_handler::Ctx,
    room::Room,
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::room::member::{MembershipState, StrippedRoomMemberEvent},
    },
    Client, RoomType,
};
use tokio::sync::mpsc;

use super::{get_client_for_user, Store};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_repeated_join_leave() -> Result<()> {
    // FIXME: Run once with memory, once with sled
    let peter = get_client_for_user(Store::Memory, "peter".to_owned()).await?;
    let karl = get_client_for_user(Store::Sled, "karl".to_owned()).await?;
    let karl_id = karl.user_id().expect("karl has a userid!").to_owned();

    // Create a room and invite karl.
    let invites = [karl_id.clone()];
    let request = assign!(CreateRoomRequest::new(), {
        invite: &invites,
        is_direct: true,
    });

    // Sync after 1 second to so that create_room receives the event it is waiting
    // for.
    let peter_clone = peter.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        peter_clone.sync_once(Default::default()).await
    });

    let created_room = peter.create_room(request).await?;
    let room_id = created_room.room_id();

    // Sync karl once to ensure he got the invite.
    karl.sync_once(Default::default()).await?;

    // Continuously sync karl from now on.
    let karl_clone = karl.clone();
    let join_handle = tokio::spawn(async move {
        karl_clone.sync(Default::default()).await;
    });
    let (invite_signal_sender, mut invite_signal) = mpsc::channel::<()>(1);
    karl.add_event_handler_context(invite_signal_sender);
    karl.add_event_handler(signal_on_invite);

    for i in 0..3 {
        println!("Iteration {i}");

        // Test that karl has the expected state in its client.
        assert!(karl.get_invited_room(room_id).is_some());
        assert!(karl.get_joined_room(room_id).is_none());
        assert!(karl.get_left_room(room_id).is_none());

        let room = karl.get_room(room_id).expect("karl has the room");
        let membership = room.get_member_no_sync(&karl_id).await?.expect("karl was invited");
        assert_eq!(*membership.membership(), MembershipState::Invite);

        // Join the room
        let room =
            karl.get_invited_room(room_id).expect("karl has the room").accept_invitation().await?;
        let membership = room.get_member_no_sync(&karl_id).await?.expect("karl joined");
        assert_eq!(*membership.membership(), MembershipState::Join);

        assert!(karl.get_invited_room(room_id).is_none());
        assert!(karl.get_joined_room(room_id).is_some());
        assert!(karl.get_left_room(room_id).is_none());

        // Leave the room
        let room = room.leave().await?;
        let membership = room.get_member_no_sync(&karl_id).await?.expect("karl left");
        assert_eq!(*membership.membership(), MembershipState::Leave);

        assert!(karl.get_invited_room(room_id).is_none());
        assert!(karl.get_joined_room(room_id).is_none());
        assert!(karl.get_left_room(room_id).is_some());

        // Invite karl again and wait for karl to receive the invite.
        let room = peter.get_joined_room(room_id).expect("peter created the room!");
        room.invite_user_by_id(&karl_id).await?;
        invite_signal.recv().await.expect("sender must be open");
    }

    // Stop the sync.
    join_handle.abort();

    // Now check the underlying state store that it also has the correct information
    // (for when the client restarts).
    let invited = karl.store().get_invited_user_ids(room_id).await?;
    assert_eq!(invited.len(), 1);
    assert_eq!(invited[0], karl_id);

    let joined = karl.store().get_joined_user_ids(room_id).await?;
    assert!(!joined.contains(&karl_id));

    let event =
        karl.store().get_member_event(room_id, &karl_id).await?.expect("member event should exist");
    assert_eq!(*event.membership(), MembershipState::Invite);

    // Yay, test succeeded
    Ok(())
}

async fn signal_on_invite(
    event: StrippedRoomMemberEvent,
    room: Room,
    client: Client,
    sender: Ctx<mpsc::Sender<()>>,
) {
    let own_id = client.user_id().expect("client is logged in");
    if event.sender == own_id {
        return;
    }

    if room.room_type() != RoomType::Invited {
        return;
    }

    if event.content.membership != MembershipState::Invite {
        return;
    }

    let invited = &event.state_key;
    if invited != own_id {
        return;
    }

    // Send signal that we received an invite.
    sender.send(()).await.expect("receiver must be open");
}
