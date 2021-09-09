#![allow(dead_code)]

use crate::room::{Room, RoomId, WeakRoom};
use crate::worker::WorkerPool;
use mediasoup::prelude::*;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Default, Clone)]
pub struct RoomsRegistry {
    // We store `WeakRoom` instead of full `Room` to avoid cycles and to not prevent rooms from
    // being destroyed when last participant disconnects
    rooms: Arc<Mutex<HashMap<RoomId, WeakRoom>>>,
}

impl RoomsRegistry {
    /// Retrieves existing room or creates a new one with specified `RoomId`
    pub async fn get_or_create_room(
        &self,
        worker: &Worker,
        room_id: RoomId,
    ) -> Result<Room, String> {
        let mut rooms = self.rooms.lock().await;
        match rooms.entry(room_id) {
            Entry::Occupied(mut entry) => match entry.get().upgrade() {
                Some(room) => Ok(room),
                None => {
                    let room = Room::new_with_id(worker, room_id).await?;
                    entry.insert(room.downgrade());
                    room.on_close({
                        let room_id = room.id();
                        let rooms = Arc::clone(&self.rooms);

                        // move || {
                        //     std::thread::spawn(move || {
                        //         futures_lite::future::block_on(async move {
                        //             rooms.lock().await.remove(&room_id);
                        //         });
                        //     });
                        // }
                        move || {
                            tokio::spawn(async move {
                                rooms.lock().await.remove(&room_id);
                            });
                        }
                    })
                    .detach();
                    Ok(room)
                }
            },
            Entry::Vacant(entry) => {
                let room = Room::new_with_id(worker, room_id).await?;
                entry.insert(room.downgrade());
                room.on_close({
                    let room_id = room.id();
                    let rooms = Arc::clone(&self.rooms);

                    move || {
                        tokio::spawn(async move {
                            rooms.lock().await.remove(&room_id);
                        });
                    }
                })
                .detach();
                Ok(room)
            }
        }
    }

    /// Create new room with random `RoomId`
    pub async fn create_room(&self, worker: &Worker) -> Result<Room, String> {
        let mut rooms = self.rooms.lock().await;
        let room = Room::new(worker).await?;
        rooms.insert(room.id(), room.downgrade());
        room.on_close({
            let room_id = room.id();
            let rooms = Arc::clone(&self.rooms);

            move || {
                tokio::spawn(async move {
                    rooms.lock().await.remove(&room_id);
                });
            }
        })
        .detach();
        Ok(room)
    }
}

#[derive(Clone)]
pub struct ServerState {
    pub worker_pool: WorkerPool,
    pub rooms_registry: RoomsRegistry,
}
