# -*- coding: utf-8 -*-
# Copyright (c) 2020 UMD Database Group. All rights reserved.
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

from boto import kinesis
import time

kinesis = kinesis.connect_to_region("us-east-1")
shard_id = 'shardId-000000000000'  #we only have one shard!
shard_it = kinesis.get_shard_iterator("joinResults", shard_id,
                                      "LATEST")["ShardIterator"]
while 1 == 1:
    out = kinesis.get_records(shard_it, limit=1)
    if len(out["Records"]) != 0:
        print(out["Records"][0]["Data"])

    shard_it = out["NextShardIterator"]
    time.sleep(0.2)