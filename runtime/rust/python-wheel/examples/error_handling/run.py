import asyncio
import random
import string

import uvloop
from client import init as client_init
from nova_distributed import DistributedRuntime, nova_worker
from server import init as server_init


def random_string(length=10):
    chars = string.ascii_letters + string.digits  # a-z, A-Z, 0-9
    return "".join(random.choices(chars, k=length))


@nova_worker()
async def worker(runtime: DistributedRuntime):
    ns = random_string()
    task = asyncio.create_task(server_init(runtime, ns))
    await client_init(runtime, ns)
    runtime.shutdown()
    await task


if __name__ == "__main__":
    uvloop.install()
    asyncio.run(worker())
