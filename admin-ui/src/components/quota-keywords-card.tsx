import { useState, useEffect } from 'react'
import { ShieldAlert, X } from 'lucide-react'
import { toast } from 'sonner'
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { useQuotaKeywords, useSetQuotaKeywords } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'

/**
 * 额度用尽判定关键词设置卡片
 *
 * 上游 402 响应体只要包含列表中任意一个关键词（子串匹配），即判定为
 * 额度用尽，触发自动禁用凭据并切换。默认内置 MONTHLY_REQUEST_COUNT，
 * 可自行增删（含内置项），以应对官方随时可能更改的错误文案。
 */
export function QuotaKeywordsCard() {
  const { data, isLoading } = useQuotaKeywords()
  const { mutate: save, isPending } = useSetQuotaKeywords()

  // 本地编辑状态
  const [keywords, setKeywords] = useState<string[]>([])
  const [newKeyword, setNewKeyword] = useState('')

  // 同步服务端数据到本地编辑状态
  useEffect(() => {
    if (data) {
      setKeywords(data.keywords)
    }
  }, [data])

  const handleAdd = () => {
    const trimmed = newKeyword.trim()
    if (!trimmed) {
      return
    }
    if (keywords.includes(trimmed)) {
      toast.error('该关键词已存在')
      return
    }
    setKeywords([...keywords, trimmed])
    setNewKeyword('')
  }

  const handleRemove = (keyword: string) => {
    setKeywords(keywords.filter((k) => k !== keyword))
  }

  const handleSave = () => {
    save(keywords, {
      onSuccess: () => toast.success('额度用尽关键词已保存'),
      onError: (error) => toast.error(`保存失败: ${extractErrorMessage(error)}`),
    })
  }

  return (
    <Card className="mt-6">
      <CardHeader>
        <CardTitle className="text-lg flex items-center gap-2">
          <ShieldAlert className="h-5 w-5" />
          额度用尽判定关键词
        </CardTitle>
        <CardDescription>
          上游返回 402 且响应体包含以下任意关键词时，判定为额度用尽，自动禁用该凭据并切换到下一个。
          官方错误文案变化时可在此增删关键词（包括内置项），无需重启或改代码。
        </CardDescription>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <p className="text-sm text-muted-foreground">加载中...</p>
        ) : (
          <div className="space-y-2">
            {keywords.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                暂无关键词，所有 402 都不会被自动判定为额度用尽
              </p>
            ) : (
              keywords.map((keyword) => (
                <div key={keyword} className="flex items-center gap-2">
                  <Input value={keyword} readOnly disabled={isPending} className="font-mono text-sm" />
                  <Button
                    variant="outline"
                    size="icon"
                    onClick={() => handleRemove(keyword)}
                    disabled={isPending}
                    aria-label={`删除关键词 ${keyword}`}
                  >
                    <X className="h-4 w-4" />
                  </Button>
                </div>
              ))
            )}
          </div>
        )}
        <div className="mt-4 flex items-center gap-2">
          <Input
            placeholder="新增关键词，如 OVERAGE_REQUEST_LIMIT_EXCEEDED"
            value={newKeyword}
            disabled={isLoading || isPending}
            onChange={(e) => setNewKeyword(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') {
                e.preventDefault()
                handleAdd()
              }
            }}
          />
          <Button variant="outline" onClick={handleAdd} disabled={isLoading || isPending}>
            添加
          </Button>
        </div>
        <div className="mt-4 flex justify-end">
          <Button onClick={handleSave} disabled={isLoading || isPending} size="sm">
            {isPending ? '保存中...' : '保存'}
          </Button>
        </div>
      </CardContent>
    </Card>
  )
}
