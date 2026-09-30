#!/usr/bin/env python3
"""Read allocated physical bytes independently of journal raw_length."""
import json
import hashlib
import hmac
import os
import subprocess
from datetime import datetime, timezone
from urllib.parse import quote, urlencode, urlsplit
from urllib.request import Request, urlopen
from xml.etree import ElementTree
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path


def physical(root):
    directories = {}
    for path in root.rglob('*'):
        if path.is_file() and not path.is_symlink():
            name = str(path.relative_to(root).parent)
            try:
                size = path.stat().st_blocks * 512
            except FileNotFoundError:
                continue
            directories[name] = directories.get(name, 0) + size
    return {'allocated_bytes': sum(directories.values()), 'directories': directories,
            'snapshot': snapshot_objects() if os.environ.get('S3_ENDPOINT') else None}


def snapshot_objects():
    """List only the snapshot prefix with signed ListObjectsV2 requests."""
    endpoint = os.environ['S3_ENDPOINT'].rstrip('/')
    bucket, prefix, region = (os.environ[key] for key in ('S3_BUCKET', 'S3_SNAPSHOT_PREFIX', 'S3_REGION'))
    access, secret = (os.environ[key] for key in ('S3_ACCESS_KEY', 'S3_SECRET_KEY'))
    host = urlsplit(endpoint).netloc
    uri = urlsplit(endpoint).path + '/' + quote(bucket, safe='')
    total = count = 0
    token = None
    seen_tokens = set()
    while True:
        query = {'list-type': '2', 'prefix': prefix.rstrip('/') + '/', 'max-keys': '1000'}
        if token:
            query['continuation-token'] = token
        query = urlencode(sorted(query.items()), quote_via=quote)
        now = datetime.now(timezone.utc)
        date, stamp = now.strftime('%Y%m%d'), now.strftime('%Y%m%dT%H%M%SZ')
        digest = hashlib.sha256(b'').hexdigest()
        headers = f'host:{host}\nx-amz-content-sha256:{digest}\nx-amz-date:{stamp}\n'
        signed = 'host;x-amz-content-sha256;x-amz-date'
        canonical = f'GET\n{uri}\n{query}\n{headers}\n{signed}\n{digest}'
        scope = f'{date}/{region}/s3/aws4_request'
        string = f'AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{hashlib.sha256(canonical.encode()).hexdigest()}'
        def sign(key, value):
            return hmac.new(key, value.encode(), hashlib.sha256).digest()
        key = sign(sign(sign(sign(('AWS4' + secret).encode(), date), region), 's3'), 'aws4_request')
        signature = hmac.new(key, string.encode(), hashlib.sha256).hexdigest()
        auth = f'AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed}, Signature={signature}'
        request = Request(f'{endpoint}/{quote(bucket, safe="")}?{query}', headers={
            'Authorization': auth, 'x-amz-date': stamp, 'x-amz-content-sha256': digest})
        with urlopen(request, timeout=5) as response:
            document = ElementTree.fromstring(response.read())
        for row in document.iter():
            if row.tag.rsplit('}', 1)[-1] == 'Contents':
                fields = {child.tag.rsplit('}', 1)[-1]: child.text for child in row}
                total += int(fields['Size'])
                count += 1
        fields = {child.tag.rsplit('}', 1)[-1]: child.text for child in document}
        if fields.get('IsTruncated') != 'true':
            break
        token = fields.get('NextContinuationToken')
        if not token or token in seen_tokens:
            raise ValueError('snapshot listing has no advancing continuation token')
        seen_tokens.add(token)
    return {'bytes': total, 'objects': count, 'prefix': prefix}


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/health':
            self.send_response(200)
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        if self.path != '/physical':
            self.send_error(404)
            return
        try:
            value = physical(Path('/data'))
            for name, query in [('node_epochs', 'SELECT name, gen_node_id, address FROM nodes'),
                                ('leader_epochs', 'SELECT partition_id, leader_gen_node_id, leader_epoch FROM partitions')]:
                result = subprocess.run(['restatectl', 'sql', '--json', '--request-timeout', '5000', query],
                                        capture_output=True, text=True, check=True, timeout=7)
                value[name] = json.loads(result.stdout)
            body = json.dumps(value).encode()
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            self.send_error(503, str(error))
            return
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


if __name__ == '__main__':
    HTTPServer(('0.0.0.0', 18102), Handler).serve_forever()
