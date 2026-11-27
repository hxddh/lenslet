"""Phases of a session against Lenslet, and a dump of what the account holds.

python rewind.py one   -> build things (S3 -> Lambda -> DynamoDB chain included)
python rewind.py two   -> change them
python rewind.py deny  -> a function denied by its role; prints "denied" or "ok"
python rewind.py invoke -> invoke that function again
python rewind.py crash  -> a function that fails, invoked and by a notification
python rewind.py policy -> print its role's inline policies
python rewind.py dump  -> print the account's contents as JSON
"""
import io, json, os, sys, time, zipfile

import boto3
from botocore.config import Config

c = dict(endpoint_url=os.environ["AWS_ENDPOINT_URL"], region_name="us-east-1",
         aws_access_key_id="x", aws_secret_access_key="x", config=Config(retries={"max_attempts": 1}))
s3, ddb, sqs, lam, iam, sns, kms, sfn, secrets = (boto3.client(n, **c) for n in (
    "s3", "dynamodb", "sqs", "lambda", "iam", "sns", "kms", "stepfunctions", "secretsmanager"))

CODE = '''
import boto3
ddb = boto3.client("dynamodb")
def handler(event, context):
    for r in event.get("Records", []):
        ddb.put_item(TableName="images", Item={"id": {"S": r["s3"]["object"]["key"]}})
'''

def one():
    ddb.create_table(TableName="orders", KeySchema=[{"AttributeName": "id", "KeyType": "HASH"}],
                     AttributeDefinitions=[{"AttributeName": "id", "AttributeType": "S"}], BillingMode="PAY_PER_REQUEST")
    ddb.create_table(TableName="images", KeySchema=[{"AttributeName": "id", "KeyType": "HASH"}],
                     AttributeDefinitions=[{"AttributeName": "id", "AttributeType": "S"}], BillingMode="PAY_PER_REQUEST")
    for t in ("orders", "images"):
        ddb.get_waiter("table_exists").wait(TableName=t, WaiterConfig={"Delay": 1})
    for i in range(3):
        ddb.put_item(TableName="orders", Item={"id": {"S": f"o-{i}"}, "total": {"N": str(i * 10)}})
    ddb.update_item(TableName="orders", Key={"id": {"S": "o-1"}}, UpdateExpression="SET paid = :t",
                    ExpressionAttributeValues={":t": {"BOOL": True}})
    s3.create_bucket(Bucket="assets")
    s3.put_object(Bucket="assets", Key="a.txt", Body=b"alpha")
    s3.put_object(Bucket="assets", Key="b.bin", Body=bytes(range(256)))
    # A multipart upload: its id is made up anew when the account is rebuilt.
    up = s3.create_multipart_upload(Bucket="assets", Key="big.bin")["UploadId"]
    part = s3.upload_part(Bucket="assets", Key="big.bin", UploadId=up, PartNumber=1, Body=b"multi" * 1000)
    s3.complete_multipart_upload(Bucket="assets", Key="big.bin", UploadId=up,
                                 MultipartUpload={"Parts": [{"ETag": part["ETag"], "PartNumber": 1}]})
    q = sqs.create_queue(QueueName="jobs")["QueueUrl"]
    # A message taken and deleted stays gone: its receipt handle is made up anew.
    sqs.send_message(QueueUrl=q, MessageBody="job 0")
    taken = sqs.receive_message(QueueUrl=q, MaxNumberOfMessages=1)["Messages"][0]
    sqs.delete_message(QueueUrl=q, ReceiptHandle=taken["ReceiptHandle"])
    sqs.send_message(QueueUrl=q, MessageBody="job 1")
    sns.create_topic(Name="news")
    # A KMS key's id is made up anew too; its alias names the new one.
    key = kms.create_key(Description="app")["KeyMetadata"]["KeyId"]
    kms.create_alias(AliasName="alias/app", TargetKeyId=key)
    role = iam.create_role(RoleName="fn-role", AssumeRolePolicyDocument=json.dumps({"Version": "2012-10-17", "Statement": [
        {"Effect": "Allow", "Principal": {"Service": ["lambda.amazonaws.com", "states.amazonaws.com"]},
         "Action": "sts:AssumeRole"}]}))["Role"]["Arn"]
    iam.put_role_policy(RoleName="fn-role", PolicyName="ddb", PolicyDocument=json.dumps({"Version": "2012-10-17", "Statement": [
        {"Effect": "Allow", "Action": "dynamodb:PutItem", "Resource": "*"},
        {"Effect": "Allow", "Action": ["sqs:ReceiveMessage", "sqs:DeleteMessage", "sqs:GetQueueAttributes"],
         "Resource": "*"},
        {"Effect": "Allow", "Action": ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"],
         "Resource": "*"}]}))
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("app.py", CODE)
    fn = lam.create_function(FunctionName="index", Runtime="python3.12", Role=role, Handler="app.handler",
                             Code={"ZipFile": buf.getvalue()})["FunctionArn"]
    lam.get_waiter("function_active_v2").wait(FunctionName="index", WaiterConfig={"Delay": 1})
    s3.create_bucket(Bucket="uploads")
    lam.add_permission(FunctionName="index", StatementId="s3", Action="lambda:InvokeFunction",
                       Principal="s3.amazonaws.com", SourceArn="arn:aws:s3:::uploads")
    s3.put_bucket_notification_configuration(Bucket="uploads", NotificationConfiguration={
        "LambdaFunctionConfigurations": [{"LambdaFunctionArn": fn, "Events": ["s3:ObjectCreated:*"]}]})
    s3.put_object(Bucket="uploads", Key="cat.jpg", Body=b"meow")
    made_up(role)
    time.sleep(2)

NOOP = '''
def handler(event, context):
    return len(event.get("Records", []))
'''

def zipped(code):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("app.py", code)
    return buf.getvalue()

def made_up(role):
    """Ids the emulator makes up, named by later calls: rebuilt, they are new."""
    # A subscription, its attribute set, and another unsubscribed.
    topic = sns.create_topic(Name="alerts")["TopicArn"]
    inbox = sqs.create_queue(QueueName="inbox")["QueueUrl"]
    inbox_arn = sqs.get_queue_attributes(QueueUrl=inbox, AttributeNames=["QueueArn"])["Attributes"]["QueueArn"]
    sub = sns.subscribe(TopicArn=topic, Protocol="sqs", Endpoint=inbox_arn, ReturnSubscriptionArn=True)["SubscriptionArn"]
    sns.set_subscription_attributes(SubscriptionArn=sub, AttributeName="RawMessageDelivery", AttributeValue="true")
    gone = sns.subscribe(TopicArn=topic, Protocol="sqs", Endpoint=inbox_arn + "-x", ReturnSubscriptionArn=True)["SubscriptionArn"]
    sns.unsubscribe(SubscriptionArn=gone)
    # An event source mapping, changed after.
    lam.create_function(FunctionName="noop", Runtime="python3.12", Role=role, Handler="app.handler",
                        Code={"ZipFile": zipped(NOOP)})
    lam.get_waiter("function_active_v2").wait(FunctionName="noop", WaiterConfig={"Delay": 1})
    feed = sqs.create_queue(QueueName="feed")["QueueUrl"]
    feed_arn = sqs.get_queue_attributes(QueueUrl=feed, AttributeNames=["QueueArn"])["Attributes"]["QueueArn"]
    esm = lam.create_event_source_mapping(EventSourceArn=feed_arn, FunctionName="noop", BatchSize=10)["UUID"]
    while lam.get_event_source_mapping(UUID=esm)["State"] != "Enabled":
        time.sleep(0.5)
    lam.update_event_source_mapping(UUID=esm, BatchSize=5)
    # An execution named by the emulator, stopped after.
    machine = sfn.create_state_machine(name="waiter", roleArn=role, definition=json.dumps(
        {"StartAt": "Wait", "States": {"Wait": {"Type": "Wait", "Seconds": 300, "End": True}}}))["stateMachineArn"]
    run = sfn.start_execution(stateMachineArn=machine)["executionArn"]
    sfn.stop_execution(executionArn=run)
    # A secret's versions.
    secrets.create_secret(Name="db", SecretString="v1")
    secrets.put_secret_value(SecretId="db", SecretString="v2")

def two():
    ddb.delete_item(TableName="orders", Key={"id": {"S": "o-0"}})
    ddb.put_item(TableName="orders", Item={"id": {"S": "o-9"}})
    s3.put_object(Bucket="assets", Key="c.txt", Body=b"gamma")
    s3.delete_object(Bucket="assets", Key="a.txt")
    q = sqs.get_queue_url(QueueName="jobs")["QueueUrl"]
    sqs.send_message(QueueUrl=q, MessageBody="job 2")
    s3.put_object(Bucket="uploads", Key="dog.jpg", Body=b"woof")
    ddb.create_table(TableName="extra", KeySchema=[{"AttributeName": "id", "KeyType": "HASH"}],
                     AttributeDefinitions=[{"AttributeName": "id", "AttributeType": "S"}], BillingMode="PAY_PER_REQUEST")
    ddb.get_waiter("table_exists").wait(TableName="extra", WaiterConfig={"Delay": 1})
    time.sleep(2)

WRITER = '''
import boto3
def handler(event, context):
    boto3.client("dynamodb").put_item(TableName="orders", Item={"id": {"S": "w-1"}})
    return "written"
'''

def deny():
    """A function whose role may not write to the table: denied, until Lenslet allows it."""
    role = iam.create_role(RoleName="writer-role", AssumeRolePolicyDocument=json.dumps({"Version": "2012-10-17", "Statement": [
        {"Effect": "Allow", "Principal": {"Service": "lambda.amazonaws.com"}, "Action": "sts:AssumeRole"}]}))["Role"]["Arn"]
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("app.py", WRITER)
    lam.create_function(FunctionName="writer", Runtime="python3.12", Role=role, Handler="app.handler",
                        Code={"ZipFile": buf.getvalue()})
    lam.get_waiter("function_active_v2").wait(FunctionName="writer", WaiterConfig={"Delay": 1})
    invoke()

def invoke():
    r = lam.invoke(FunctionName="writer")
    print("denied" if r.get("FunctionError") else "ok", end="")

CRASHER = '''
def handler(event, context):
    print("about to fail")
    raise ValueError("boom")
'''

def crash():
    """A function that fails: invoked, and by a bucket's notification."""
    role = iam.get_role(RoleName="fn-role")["Role"]["Arn"]
    fn = lam.create_function(FunctionName="crasher", Runtime="python3.12", Role=role, Handler="app.handler",
                             Code={"ZipFile": zipped(CRASHER)})["FunctionArn"]
    lam.get_waiter("function_active_v2").wait(FunctionName="crasher", WaiterConfig={"Delay": 1})
    r = lam.invoke(FunctionName="crasher")
    print(r.get("FunctionError", "none"), end="")
    s3.create_bucket(Bucket="crashes")
    lam.add_permission(FunctionName="crasher", StatementId="s3", Action="lambda:InvokeFunction",
                       Principal="s3.amazonaws.com", SourceArn="arn:aws:s3:::crashes")
    s3.put_bucket_notification_configuration(Bucket="crashes", NotificationConfiguration={
        "LambdaFunctionConfigurations": [{"LambdaFunctionArn": fn, "Events": ["s3:ObjectCreated:*"]}]})
    s3.put_object(Bucket="crashes", Key="x", Body=b"x")

def policy():
    names = iam.list_role_policies(RoleName="writer-role")["PolicyNames"]
    print(json.dumps({n: iam.get_role_policy(RoleName="writer-role", PolicyName=n)["PolicyDocument"] for n in names}))

def dump():
    out = {"tables": {}, "buckets": {}, "queues": {}, "topics": [], "functions": [], "keys": [], "roles": {}}
    for t in sorted(ddb.list_tables()["TableNames"]):
        items = ddb.scan(TableName=t)["Items"]
        out["tables"][t] = sorted(json.dumps(i, sort_keys=True) for i in items)
    for b in sorted(x["Name"] for x in s3.list_buckets()["Buckets"]):
        objs = s3.list_objects_v2(Bucket=b).get("Contents", [])
        out["buckets"][b] = {o["Key"]: s3.get_object(Bucket=b, Key=o["Key"])["Body"].read().hex() for o in objs}
    for u in sorted(sqs.list_queues().get("QueueUrls", [])):
        a = sqs.get_queue_attributes(QueueUrl=u, AttributeNames=["ApproximateNumberOfMessages"])["Attributes"]
        out["queues"][u.rsplit("/", 1)[-1]] = a["ApproximateNumberOfMessages"]
    out["topics"] = sorted(t["TopicArn"] for t in sns.list_topics()["Topics"])
    out["functions"] = sorted(f["FunctionName"] for f in lam.list_functions()["Functions"])
    out["keys"] = sorted(a["AliasName"] for a in kms.list_aliases()["Aliases"]
                         if not a["AliasName"].startswith("alias/aws/") and "TargetKeyId" in a
                         and kms.describe_key(KeyId=a["AliasName"])["KeyMetadata"]["Description"] == "app")
    out["subscriptions"] = sorted(
        (x["Protocol"], x["Endpoint"].rsplit(":", 1)[-1],
         sns.get_subscription_attributes(SubscriptionArn=x["SubscriptionArn"])["Attributes"].get("RawMessageDelivery", "false"))
        for t in sns.list_topics()["Topics"]
        for x in sns.list_subscriptions_by_topic(TopicArn=t["TopicArn"])["Subscriptions"])
    out["mappings"] = sorted((m["FunctionArn"].rsplit(":", 1)[-1], m["BatchSize"])
                             for m in lam.list_event_source_mappings()["EventSourceMappings"])
    out["executions"] = sorted(e["status"] for m in sfn.list_state_machines()["stateMachines"]
                               for e in sfn.list_executions(stateMachineArn=m["stateMachineArn"])["executions"])
    out["secrets"] = sorted((x["Name"], secrets.get_secret_value(SecretId=x["Name"])["SecretString"])
                            for x in secrets.list_secrets()["SecretList"])
    for r in iam.list_roles()["Roles"]:
        out["roles"][r["RoleName"]] = sorted(iam.list_role_policies(RoleName=r["RoleName"])["PolicyNames"])
    print(json.dumps(out, sort_keys=True))

{"one": one, "two": two, "deny": deny, "invoke": invoke, "crash": crash, "policy": policy, "dump": dump}[sys.argv[1]]()
