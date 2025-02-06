# SPDX-FileCopyrightText: Copyright (c) 2024-2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
# http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
import asyncio
import os
import uuid
from datetime import datetime
from typing import AsyncIterator, Awaitable, Callable, Optional, Tuple

import nats

from triton_distributed.icp.event_plane import EventMetadata, EventTopic

DEFAULT_EVENTS_PORT = int(os.getenv("DEFAULT_EVENTS_PORT", 4222))
DEFAULT_EVENTS_HOST = os.getenv("DEFAULT_EVENTS_HOST", "localhost")
DEFAULT_EVENTS_URI = os.getenv(
    "DEFAULT_EVENTS_URI", f"nats://{DEFAULT_EVENTS_HOST}:{DEFAULT_EVENTS_PORT}"
)


class NatsEventPlane:
    """EventPlane implementation using NATS."""

    def __init__(
        self,
        server_uri: str,
        component_id: uuid.UUID,
        run_callback_in_parallel: bool = False,
    ):
        """Initialize the NATS event plane.

        Args:
            server_uri: URI of the NATS server.
            component_id: Component ID.
        """
        self._run_callback_in_parallel = run_callback_in_parallel
        self._server_uri = server_uri
        self._component_id = component_id
        self._nc = nats.NATS()

    async def connect(self):
        """Connect to the NATS server."""
        await self._nc.connect(self._server_uri)

    async def publish(
        self, event: bytes, event_type: str, event_topic: Optional[EventTopic]
    ) -> EventMetadata:
        """Publish an event to the NATS server.

        Args:
            event: Event payload.
            event_type: Type of the event.
            event_topic: EventTopic of the event.
        """
        event_metadata = EventMetadata(
            event_id=uuid.uuid4(),
            event_topic=event_topic,
            event_type=event_type,
            timestamp=datetime.utcnow(),
            component_id=self._component_id,
        )

        metadata_serialized = event_metadata.to_raw()
        metadata_size = len(metadata_serialized).to_bytes(4, byteorder="big")

        # Concatenate metadata size, metadata, and event payload
        message = metadata_size + metadata_serialized + event

        subject = self._compose_publish_subject(event_metadata)
        await self._nc.publish(subject, message)
        return event_metadata

    async def subscribe(
        self,
        callback: Callable[[bytes, bytes], Awaitable[None]],
        event_topic: Optional[EventTopic] = None,
        event_type: Optional[str] = None,
        component_id: Optional[uuid.UUID] = None,
    ):
        """Subscribe to events on the NATS server.

        Args:
            callback: Callback function to be called when an event is received.
            event_topic: Event event_topic.
            event_type: Event type.
            component_id: Component ID.
        """

        async def _message_handler(msg):
            metadata, event = self._extract_metadata_and_payload(msg.data)

            async def wrapper():
                await callback(event, metadata)  # Ensure it's a proper coroutine

            if self._run_callback_in_parallel:
                asyncio.create_task(wrapper())  # Run in parallel
            else:
                await callback(event, metadata)  # Await normally

        subject = self._compose_subscribe_subject(event_topic, event_type, component_id)
        await self._nc.subscribe(subject, cb=_message_handler)

    async def subscribe_iter(
        self,
        event_topic: Optional[EventTopic] = None,
        event_type: Optional[str] = None,
        component_id: Optional[uuid.UUID] = None,
    ) -> AsyncIterator[Tuple[bytes, bytes]]:
        """Subscribe to events on the NATS server and return an async iterator.

        Args:
            event_topic: Event event_topic.
            event_type: Event type.
            component_id: Component ID.
        """
        subject = self._compose_subscribe_subject(event_topic, event_type, component_id)
        sub = await self._nc.subscribe(subject)
        async for msg in sub.messages:
            metadata, event = self._extract_metadata_and_payload(msg.data)
            yield event, metadata

    async def disconnect(self):
        """Disconnect from the NATS server."""
        await self._nc.close()

    def _compose_publish_subject(self, event_metadata: EventMetadata):
        return f"ep.{event_metadata.event_type}.{event_metadata.component_id}.{str(event_metadata.event_topic) + '.' if event_metadata.event_topic else ''}trunk"

    def _compose_subscribe_subject(
        self,
        event_topic: Optional[EventTopic],
        event_type: Optional[str],
        component_id: Optional[uuid.UUID],
    ):
        return f"ep.{event_type or '*'}.{component_id or '*'}.{str(event_topic) + '.' if event_topic else ''}>"

    def _extract_metadata_and_payload(self, message: bytes):
        # Extract metadata size
        message_view = memoryview(message)

        metadata_size = int.from_bytes(message_view[:4], byteorder="big")

        # Extract metadata and event
        metadata_serialized = message_view[4 : 4 + metadata_size]
        event = message_view[4 + metadata_size :]

        return metadata_serialized, event
