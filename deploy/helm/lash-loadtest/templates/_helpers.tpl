{{- define "loadtest.name" -}}
{{- .Values.nameOverride | trunc 40 | trimSuffix "-" -}}
{{- end -}}
{{- define "loadtest.s3" -}}
{{- if eq .Values.s3.mode "external" -}}
{{- required "s3.externalEndpoint is required in external mode" .Values.s3.externalEndpoint -}}
{{- else -}}
http://{{ include "loadtest.name" . }}-s3:3900
{{- end -}}
{{- end -}}
{{- define "loadtest.network" -}}
{{- if .Values.network.enabled }}
initContainers:
  - name: directed-link
    image: "{{ .Values.image.repository }}:{{ .tag | default .Values.image.tag }}"
    imagePullPolicy: {{ .Values.image.pullPolicy }}
    command: [tc, qdisc, replace, dev, eth0, root, netem, delay, "{{ .Values.network.delayMs }}ms", "{{ .Values.network.jitterMs }}ms", rate, "{{ .Values.network.bandwidthMbps }}mbit"]
    securityContext:
      capabilities: {add: [NET_ADMIN]}
{{- end }}
{{- end -}}
{{- define "loadtest.env" -}}
env:
  - {name: DATABASE_URL, valueFrom: {secretKeyRef: {name: {{ .Values.credentialsSecret }}, key: database-url}}}
  - {name: WITNESS_DATABASE_URL, valueFrom: {secretKeyRef: {name: {{ .Values.credentialsSecret }}, key: witness-database-url}}}
  - {name: S3_ACCESS_KEY, valueFrom: {secretKeyRef: {name: {{ .Values.credentialsSecret }}, key: s3-access-key}}}
  - {name: S3_SECRET_KEY, valueFrom: {secretKeyRef: {name: {{ .Values.credentialsSecret }}, key: s3-secret-key}}}
  - {name: S3_ENDPOINT, value: {{ include "loadtest.s3" . | quote }}}
  - {name: S3_REGION, value: {{ .Values.s3.region | quote }}}
  - {name: S3_BUCKET, value: {{ .Values.s3.bucket | quote }}}
  - {name: S3_PREFIX, value: {{ .Values.s3.attachmentPrefix | quote }}}
  - {name: RESTATE_INGRESS_URL, value: "http://{{ include "loadtest.name" . }}-restate:8080"}
  - {name: RESTATE_ADMIN_URL, value: "http://{{ include "loadtest.name" . }}-restate:9070"}
  - {name: RESTATE_AUTHORITY_ID, value: "loadtest:{{ .Release.Namespace }}"}
  - {name: MOCK_PROVIDER_BASE_URL, value: "http://{{ include "loadtest.name" . }}-provider:18001"}
{{- end -}}
